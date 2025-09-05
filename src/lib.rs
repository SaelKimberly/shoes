mod address;
mod async_stream;
mod buf_reader;
mod client_proxy_selector;
mod copy_bidirectional;
mod copy_bidirectional_message;
mod copy_multidirectional_message;
mod http_handler;
mod hysteria2_server;
mod noop_stream;
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

pub mod config;
pub mod option_util;

pub use config::ServerConfig;

use tokio::task::JoinHandle;

use crate::{
    config::Transport, quic_server::start_quic_servers, tcp::tcp_server::start_tcp_servers,
};

pub async fn start_servers(config: ServerConfig) -> std::io::Result<Vec<JoinHandle<()>>> {
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
