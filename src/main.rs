mod address;
mod async_stream;
mod buf_reader;
mod client_proxy_selector;
mod config;
mod copy_bidirectional;
mod copy_bidirectional_message;
mod copy_multidirectional_message;
mod http_handler;
mod hysteria2_server;
mod noop_stream;
mod option_util;
mod port_forward_handler;
mod quic_server;
mod quic_stream;
mod resolver;
mod rustls_util;
mod salt_checker;
mod shadow_tls;
mod shadowsocks;
mod snell;
mod socket_util;
mod socks_handler;
mod stream_reader;
mod tcp;
mod thread_util;
mod timed_salt_checker;
mod tls_handler;
mod trojan_handler;
mod tuic_server;
mod udp_message_stream;
mod udp_multi_message_stream;
mod util;
mod vless_handler;
mod vless_message_stream;
mod vmess;
mod websocket;

use clap::Arg;
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;

use log::debug;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tcp_server::start_tcp_servers;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;

use crate::config::{ServerConfig, Transport};
use crate::quic_server::start_quic_servers;
use crate::thread_util::set_num_threads;
use tcp::*;

#[derive(Debug)]
struct ConfigChanged;

fn start_notify_thread(
    config_paths: Vec<PathBuf>,
) -> (RecommendedWatcher, UnboundedReceiver<ConfigChanged>) {
    let (tx, rx) = unbounded_channel();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(event) => {
            if matches!(event.kind, EventKind::Modify(..)) {
                tx.send(ConfigChanged {}).unwrap();
            }
        }
        Err(e) => println!("watch error: {e:?}"),
    })
    .unwrap();

    for config_path in config_paths {
        watcher
            .watch(config_path.as_path(), RecursiveMode::NonRecursive)
            .unwrap();
    }

    (watcher, rx)
}

async fn start_servers(config: ServerConfig) -> std::io::Result<Vec<JoinHandle<()>>> {
    let mut join_handles = Vec::with_capacity(3);

    match config.transport {
        Transport::Tcp => match start_tcp_servers(config.clone()).await {
            Ok(handles) => {
                join_handles.extend(handles);
            }
            Err(e) => {
                for join_handle in join_handles {
                    join_handle.abort();
                }
                return Err(e);
            }
        },
        Transport::Quic => match start_quic_servers(config.clone()).await {
            Ok(handles) => {
                join_handles.extend(handles);
            }
            Err(e) => {
                for join_handle in join_handles {
                    join_handle.abort();
                }
                return Err(e);
            }
        },
        Transport::Udp => todo!(),
    }

    if join_handles.is_empty() {
        return Err(std::io::Error::other(format!(
            "failed to start servers at {}",
            &config.bind_location
        )));
    }

    Ok(join_handles)
}

fn main() {
    env_logger::builder()
        .format(|buf, record| {
            let timestamp = buf.timestamp();
            let level_style = buf.default_level_style(record.level());
            let sanitized_args = format!("{}", record.args())
                .chars()
                .map(|c| {
                    if c.is_ascii_graphic() || c == ' ' {
                        c
                    } else {
                        '?'
                    }
                })
                .collect::<String>();

            writeln!(
                buf,
                "[{} {level_style}{}{level_style:#} {}] {}",
                timestamp,
                record.level(),
                record.target(),
                sanitized_args
            )
        })
        .init();

    let mut cmd = clap::builder::Command::new("shoes").arg(
        Arg::new("threads").short('t').long("threads").value_name("N").value_parser(clap::value_parser!(usize)).help(
            "Set the number of worker threads. This usually defaults to the number of CPUs.",
        )
    ).arg(
        Arg::new("dry-run").short('d').long("dry-run").help("Do not start any servers.")
    ).arg(
        Arg::new("config").required(true).value_parser(clap::value_parser!(PathBuf)).num_args(1..).value_name("server uri or config filename").help("Path to a YAML config file.")
    );
    cmd.build();

    let args = cmd.clone().get_matches();
    println!("Args: {args:#?}");

    let mut config_paths: Vec<PathBuf> = args
        .get_many::<PathBuf>("config")
        .unwrap()
        .cloned()
        .collect();

    let num_threads = args.get_one::<usize>("threads").unwrap_or(&0).to_owned();
    let dry_run = args.get_flag("dry-run");

    if config_paths.is_empty() {
        println!("No config specified, assuming loading from file config.shoes.yaml");
        config_paths.push(PathBuf::from_str("config.shoes.yaml").unwrap());
    }

    if dry_run {
        println!("Starting dry run.");
    }

    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let num_threads = match num_threads {
        0 => std::cmp::max(2, max_threads),
        _ => {
            if num_threads > max_threads {
                max_threads
            } else {
                num_threads
            }
        }
    };
    debug!("Runtime threads: {num_threads}");

    // Used by QUIC to figure out the number of endpoints.
    // TODO: can we pass it in instead?
    set_num_threads(num_threads);

    let mut builder = if num_threads == 1 {
        Builder::new_current_thread()
    } else {
        let mut mt = Builder::new_multi_thread();
        mt.worker_threads(num_threads);
        mt
    };

    let runtime = builder
        .enable_io()
        .enable_time()
        .build()
        .expect("Could not build tokio runtime");

    runtime.block_on(async move {
        let (_watcher, mut config_rx) = start_notify_thread(config_paths.clone());

        loop {
            let configs = match config::load_configs(&config_paths).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to load server configs: {e}\n");
                    cmd.clone().print_help().unwrap();
                    return;
                }
            };

            let (configs, load_file_count) = match config::convert_cert_paths(configs).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to load cert files: {e}\n");
                    cmd.clone().print_help().unwrap();
                    return;
                }
            };

            if load_file_count > 0 {
                    println!("Loaded {load_file_count} certs/keys from files");
            }

            for config in configs.iter() {
                debug!("================================================================================");
                debug!("{config:#?}");
            }
            debug!("================================================================================");

            if dry_run {
                if let Err(e) = config::create_server_configs(configs).await {
                    eprintln!("Dry run failed, could not create server configs: {e}\n");
                } else {
                    println!("Finishing dry run, config parsed successfully.");
                }
                return;
            }

            println!("\nStarting {} server(s)..", configs.len());

            let mut join_handles = vec![];

            let server_configs = match config::create_server_configs(configs).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to create server configs: {e}\n");
                    cmd.clone().print_help().unwrap();
                    return;
                }
            };
            for server_config in server_configs {
                join_handles.extend(start_servers(server_config).await.unwrap());
            }

            config_rx.recv().await.unwrap();

            println!("Configs changed, restarting servers in 3 seconds..");

            for join_handle in join_handles {
                join_handle.abort();
            }

            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            // Remove any extra events
            while config_rx.try_recv().is_ok() {}
        }
    });
}
