/// Shadow TLS implementation.
/// References:
/// - https://github.com/ihciah/shadow-tls
/// - https://commandlinefanatic.com/cgi-bin/showarticle.cgi?article=art080
/// - https://wiki.osdev.org/TLS_Handshake#Client_Hello_Message
/// - https://tls13.xargs.org/#client-hello/annotated
use std::fmt::Debug;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::AsyncWriteExt;

use super::shadow_tls_hmac::ShadowTlsHmac;
use super::shadow_tls_stream::ShadowTlsStream;
use crate::address::NetLocation;
use crate::async_stream::AsyncStream;
use crate::buf_reader::BufReader;
use crate::client_proxy_selector::ClientProxySelector;
use crate::noop_stream::NoopStream;
use crate::option_util::NoneOrOne;
use crate::resolver::Resolver;
use crate::stream_reader::StreamReader;
use crate::tcp_client_connector::TcpClientConnector;
use crate::tcp_handler::{TcpServerHandler, TcpServerSetupResult};
use crate::util::{allocate_vec, write_all};

// context wrapper because it's not Debug
struct ShadowTlsXorContext(aws_lc_rs::digest::Context);

impl Debug for ShadowTlsXorContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[ShadowTlsXorContext]")
    }
}

#[derive(Debug)]
pub struct ShadowTlsServerTarget {
    initial_hmac: ShadowTlsHmac,
    initial_xor_context: ShadowTlsXorContext,
    handshake: ShadowTlsServerTargetHandshake,
    handler: Box<dyn TcpServerHandler>,
    override_proxy_provider: NoneOrOne<Arc<ClientProxySelector<TcpClientConnector>>>,
}

impl ShadowTlsServerTarget {
    pub fn new(
        password: String,
        handshake: ShadowTlsServerTargetHandshake,
        handler: Box<dyn TcpServerHandler>,
        override_proxy_provider: NoneOrOne<Arc<ClientProxySelector<TcpClientConnector>>>,
    ) -> Self {
        let password_bytes = password.into_bytes();
        let hmac_key = aws_lc_rs::hmac::Key::new(
            aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            &password_bytes,
        );
        let initial_hmac = ShadowTlsHmac::new(&hmac_key);
        let mut initial_xor_context = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
        initial_xor_context.update(&password_bytes);
        Self {
            initial_hmac,
            initial_xor_context: ShadowTlsXorContext(initial_xor_context),
            handshake,
            handler,
            override_proxy_provider,
        }
    }
}

#[derive(Debug)]
pub enum ShadowTlsServerTargetHandshake {
    Local(Arc<rustls::ServerConfig>),
    Remote {
        location: NetLocation,
        client_connectors: Vec<TcpClientConnector>,
        next_proxy_index: AtomicUsize,
    },
}

impl ShadowTlsServerTargetHandshake {
    pub fn new_local(server_config: Arc<rustls::ServerConfig>) -> Self {
        ShadowTlsServerTargetHandshake::Local(server_config)
    }

    pub fn new_remote(location: NetLocation, client_connectors: Vec<TcpClientConnector>) -> Self {
        ShadowTlsServerTargetHandshake::Remote {
            location,
            client_connectors,
            next_proxy_index: AtomicUsize::new(0),
        }
    }
}

const TLS_HEADER_LEN: usize = 5;

// the limit should be 5 (header) + 2^14 + 256 (AEAD encryption overhead) = 16640,
// although draft-mattsson-tls-super-jumbo-record-limit-01 would increase that.
// we set the limit to 5 + u16::MAX to allow for the maximum possible record size.
const TLS_FRAME_MAX_LEN: usize = TLS_HEADER_LEN + 65535;

const CONTENT_TYPE_HANDSHAKE: u8 = 0x16;
const CONTENT_TYPE_APPLICATION_DATA: u8 = 0x17;

const HANDSHAKE_TYPE_SERVER_HELLO: u8 = 0x02;
const HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 0x01;

// retry request random value, see https://datatracker.ietf.org/doc/html/rfc8446#section-4.1.3
// TODO: should we also check to disallow TLS1.2/TLS1.1 client downgrade requests?
const RETRY_REQUEST_RANDOM_BYTES: [u8; 32] = [
    0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11, 0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65, 0xB8, 0x91,
    0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E, 0x07, 0x9E, 0x09, 0xE2, 0xC8, 0xA8, 0x33, 0x9C,
];

#[inline]
pub async fn setup_shadowtls_server_stream(
    server_stream: Box<dyn AsyncStream>,
    target: &ShadowTlsServerTarget,
    parsed_client_hello: ParsedClientHello,
    resolver: &Arc<dyn Resolver>,
) -> std::io::Result<TcpServerSetupResult> {
    let ParsedClientHello {
        client_hello_frame,
        client_hello_record_legacy_version_major,
        client_hello_record_legacy_version_minor,
        client_hello_content_version_major,
        client_hello_content_version_minor,
        parsed_digest,
        client_reader,
        supports_tls13: client_supports_tls13,
        ..
    } = parsed_client_hello;

    let ParsedClientHelloDigest {
        client_hello_digest,
        client_hello_digest_start_index,
        client_hello_digest_end_index,
    } = match parsed_digest {
        Some(d) => d,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "client did not send a 32-byte session id",
            ));
        }
    };

    if !client_supports_tls13 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "client does not support TLS1.3",
        ));
    }

    if client_hello_record_legacy_version_major != 3
        || client_hello_record_legacy_version_minor != 1
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "expected client TLS record protocol 1.0 (major/minor 3.1), got major/minor {client_hello_record_legacy_version_major}.{client_hello_record_legacy_version_minor}"
            ),
        ));
    }

    if client_hello_content_version_major != 3 || client_hello_content_version_minor != 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "expected client TLS content protocol 1.2 (major/minor 3.3), got major/minor {client_hello_content_version_major}.{client_hello_content_version_minor}"
            ),
        ));
    }

    // verify the hmac digest
    let mut hmac_client_hello = target.initial_hmac.clone();
    hmac_client_hello.update(&client_hello_frame[TLS_HEADER_LEN..client_hello_digest_start_index]);
    hmac_client_hello.update(&[0; 4]);
    hmac_client_hello.update(&client_hello_frame[client_hello_digest_end_index..]);

    if client_hello_digest != hmac_client_hello.finalized_digest() {
        // TODO: forward to handshake server
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "hmac tag mismatch",
        ));
    }

    let shadow_tls_stream = match target.handshake {
        ShadowTlsServerTargetHandshake::Remote {
            ref location,
            ref client_connectors,
            ref next_proxy_index,
        } => {
            let index = next_proxy_index.fetch_add(1, Ordering::Relaxed);
            let client_connector = &client_connectors[index % client_connectors.len()];
            setup_remote_handshake(
                server_stream,
                client_reader,
                client_hello_frame,
                &target.initial_hmac,
                &target.initial_xor_context,
                location.clone(),
                client_connector,
                resolver,
            )
            .await
            .map_err(|e| std::io::Error::other(format!("failed to setup remote handshake: {e}")))?
        }
        ShadowTlsServerTargetHandshake::Local(ref local_config) => setup_local_handshake(
            server_stream,
            client_reader,
            client_hello_frame,
            &target.initial_hmac,
            &target.initial_xor_context,
            local_config.clone(),
        )
        .await
        .map_err(|e| std::io::Error::other(format!("failed to setup local handshake: {e}")))?,
    };

    let mut target_setup_result = target
        .handler
        .setup_server_stream(Box::new(shadow_tls_stream))
        .await
        .map_err(|e| {
            std::io::Error::other(format!(
                "failed to setup server stream after shadow tls: {e}"
            ))
        });

    if let Ok(ref mut setup_result) = target_setup_result {
        // TODO: do we need initial flush?
        if setup_result.override_proxy_provider_unspecified()
            && !target.override_proxy_provider.is_unspecified()
        {
            setup_result.set_override_proxy_provider(target.override_proxy_provider.clone());
        }
    }

    target_setup_result
}

pub struct ParsedClientHello {
    pub client_hello_frame: Vec<u8>,
    pub client_hello_record_legacy_version_major: u8,
    pub client_hello_record_legacy_version_minor: u8,
    pub client_hello_content_version_major: u8,
    pub client_hello_content_version_minor: u8,
    pub parsed_digest: Option<ParsedClientHelloDigest>,
    pub client_reader: StreamReader,
    pub requested_server_name: Option<String>,
    pub supports_tls13: bool,
}

pub struct ParsedClientHelloDigest {
    pub client_hello_digest: Vec<u8>,
    pub client_hello_digest_start_index: usize,
    pub client_hello_digest_end_index: usize,
}

#[inline]
pub async fn read_client_hello(
    server_stream: &mut Box<dyn AsyncStream>,
) -> std::io::Result<ParsedClientHello> {
    // enough for tls header + a max tls payload
    let mut client_reader = StreamReader::new_with_buffer_size(TLS_FRAME_MAX_LEN);

    // read the first tls frame, allocate so that we can borrow and use the payload below
    let client_tls_header_bytes = client_reader
        .read_slice(server_stream, TLS_HEADER_LEN)
        .await?
        .to_vec();

    let client_content_type = client_tls_header_bytes[0];
    if client_content_type != CONTENT_TYPE_HANDSHAKE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected client handshake",
        ));
    }

    let client_legacy_version_major = client_tls_header_bytes[1];
    let client_legacy_version_minor = client_tls_header_bytes[2];

    let client_payload_len =
        u16::from_be_bytes([client_tls_header_bytes[3], client_tls_header_bytes[4]]) as usize;
    let client_payload_bytes = client_reader
        .read_slice(server_stream, client_payload_len)
        .await?;

    let mut client_hello = BufReader::new(client_payload_bytes);
    if client_hello.read_u8()? != HANDSHAKE_TYPE_CLIENT_HELLO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected ClientHello",
        ));
    }

    let client_hello_message_len = client_hello.read_u24_be()? as usize;
    // this should be 4 bytes less than the payload length (handshake type + 3 bytes length)
    if client_hello_message_len + 4 != client_payload_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "client hello message length mismatch",
        ));
    }

    let client_version_major = client_hello.read_u8()?;
    let client_version_minor = client_hello.read_u8()?;
    let record_protocol_version_ok = client_version_major == 0x03
        && (client_version_minor == 0x01 || client_version_minor == 0x03);
    if !record_protocol_version_ok {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "unexpected ClientHello TLS version {client_version_major}.{client_version_minor}"
            ),
        ));
    }

    // skip client random
    client_hello.skip(32)?;

    let client_session_id_len = client_hello.read_u8()?;

    let parsed_digest = if client_session_id_len == 32 {
        let client_session_id = client_hello.read_slice(32)?;

        // save the hmac digest and session id position for validation once we know the server name
        let client_hello_digest = client_session_id[28..].to_vec();
        let post_session_id_index = client_hello.position();

        let client_hello_digest_start_index = TLS_HEADER_LEN + post_session_id_index - 4;
        let client_hello_digest_end_index = TLS_HEADER_LEN + post_session_id_index;

        Some(ParsedClientHelloDigest {
            client_hello_digest,
            client_hello_digest_start_index,
            client_hello_digest_end_index,
        })
    } else {
        if client_session_id_len > 0 {
            client_hello.skip(client_session_id_len as usize)?;
        }
        None
    };

    let client_cipher_suite_len = client_hello.read_u16_be()?;
    client_hello.skip(client_cipher_suite_len as usize)?;

    let client_compression_method_len = client_hello.read_u8()?;
    client_hello.skip(client_compression_method_len as usize)?;

    let client_extensions_len = client_hello.read_u16_be()?;
    let client_extension_bytes = client_hello.read_slice(client_extensions_len as usize)?;

    let mut client_extensions = BufReader::new(client_extension_bytes);

    let mut requested_server_name: Option<String> = None;
    let mut client_supports_tls13 = false;

    while !client_extensions.is_consumed() {
        let extension_type = client_extensions.read_u16_be()?;
        let extension_len = client_extensions.read_u16_be()? as usize;

        if extension_type == 0x0000 {
            // server name
            if requested_server_name.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "multiple server names",
                ));
            }
            // TODO: assert lengths
            let _server_name_list_len = client_extensions.read_u16_be()?;
            let server_name_type = client_extensions.read_u8()?;
            if server_name_type != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "expected server name type to be hostname (0)",
                ));
            }
            let server_name_len = client_extensions.read_u16_be()?;
            let server_name_str = client_extensions.read_str(server_name_len as usize)?;
            requested_server_name = Some(server_name_str.to_string());
        } else if extension_type == 0x002b {
            // supported versions
            let version_list_len = client_extensions.read_u8()?;
            if version_list_len % 2 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid odd version list length: 0x{version_list_len:02x}"),
                ));
            }
            let version_list_bytes = client_extensions.read_slice(version_list_len as usize)?;
            for i in (0..version_list_bytes.len()).step_by(2) {
                let version_major = version_list_bytes[i];
                let version_minor = version_list_bytes[i + 1];
                if version_major == 3 && version_minor == 4 {
                    client_supports_tls13 = true;
                    break;
                }
            }
        } else {
            client_extensions.skip(extension_len)?;
        }
    }

    let mut client_hello_frame =
        Vec::with_capacity(client_tls_header_bytes.len() + client_payload_bytes.len());
    client_hello_frame.extend_from_slice(&client_tls_header_bytes);
    client_hello_frame.extend_from_slice(client_payload_bytes);

    Ok(ParsedClientHello {
        client_hello_frame,
        client_hello_record_legacy_version_major: client_legacy_version_major,
        client_hello_record_legacy_version_minor: client_legacy_version_minor,
        client_hello_content_version_major: client_version_major,
        client_hello_content_version_minor: client_version_minor,
        parsed_digest,
        client_reader,
        requested_server_name,
        supports_tls13: client_supports_tls13,
    })
}

pub struct ParsedServerHello {
    pub server_random: Vec<u8>,
}

pub fn parse_server_hello(server_hello_frame: &[u8]) -> std::io::Result<ParsedServerHello> {
    // we don't need to validate that the frame is large enough to contain the header, because
    // a full header was read in order to successfully read the complete frame that is passed in
    // to this function.
    let server_content_type = server_hello_frame[0];
    if server_content_type != CONTENT_TYPE_HANDSHAKE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected server handshake",
        ));
    }

    let server_legacy_version_major = server_hello_frame[1];
    let server_legacy_version_minor = server_hello_frame[2];
    if server_legacy_version_major != 3 || server_legacy_version_minor != 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "unexpected server TLS version {server_legacy_version_major}.{server_legacy_version_minor}"
            ),
        ));
    }

    // we don't need to validate the frame payload length, because this value was used to
    // read the complete frame that is passed in to this function.
    let server_payload_len =
        u16::from_be_bytes([server_hello_frame[3], server_hello_frame[4]]) as usize;

    let server_handshake_type = server_hello_frame[5];
    if server_handshake_type != HANDSHAKE_TYPE_SERVER_HELLO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected ServerHello",
        ));
    }

    let server_hello_message_len = u32::from_be_bytes([
        0,
        server_hello_frame[6],
        server_hello_frame[7],
        server_hello_frame[8],
    ]) as usize;

    // this should be 4 bytes less than the payload length (handshake type + 3 bytes length)
    if server_hello_message_len + 4 != server_payload_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server hello message length mismatch",
        ));
    }

    let server_version_major = server_hello_frame[9];
    let server_version_minor = server_hello_frame[10];
    if server_version_major != 3 || server_version_minor != 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "expected TLS 1.2 (major/minor 3.3), got major/minor {server_version_major}.{server_version_minor}"
            ),
        ));
    }

    let server_random = &server_hello_frame[11..43];
    if server_random == RETRY_REQUEST_RANDOM_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server sent a HelloRetryRequest",
        ));
    }
    let server_random = server_random.to_vec();

    let server_session_id_len = server_hello_frame[43];
    if server_session_id_len != 32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected session id len 32, got {server_session_id_len}"),
        ));
    }

    // skip unused fields:
    //   let _server_session_id = &server_hello_frame[44..76];
    //   let _server_selected_cipher_suite =
    //     u16::from_be_bytes([server_hello_frame[76], server_hello_frame[77]]);
    //   let _server_compression_method = server_hello_frame[78];

    // this length needs to be validated because it is unchecked when reading the complete
    // frame that is passed in to this function.
    let server_extensions_len =
        u16::from_be_bytes([server_hello_frame[79], server_hello_frame[80]]) as usize;
    if server_hello_frame.len() < 81 + server_extensions_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server hello message too short for extensions",
        ));
    }

    let server_extension_bytes = &server_hello_frame[81..81 + server_extensions_len];

    let mut server_extensions = BufReader::new(server_extension_bytes);
    let mut server_has_supported_version = false;
    while !server_extensions.is_consumed() {
        let extension_type = server_extensions.read_u16_be().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to read extension type from ServerHello: {e}"),
            )
        })?;
        let extension_len = server_extensions.read_u16_be().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to read extension length from ServerHello: {e}"),
            )
        })? as usize;

        if extension_type == 0x002b {
            // supported versions
            let version_bytes = server_extensions.read_slice(2).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("failed to read supported version from ServerHello: {e}"),
                )
            })?;
            if version_bytes[0] != 3 && version_bytes[1] != 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "expected server supported version to be TLS 1.3 (0x0304), got 0x{:02x}{:02x}",
                        version_bytes[0], version_bytes[1]
                    ),
                ));
            }
            server_has_supported_version = true;
        } else {
            server_extensions.skip(extension_len).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("failed to skip extension in ServerHello: {e}"),
                )
            })?;
        }
    }

    if !server_has_supported_version {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server did not have supported versions extension",
        ));
    }

    Ok(ParsedServerHello { server_random })
}

#[allow(clippy::too_many_arguments)]
#[inline]
async fn setup_remote_handshake(
    mut server_stream: Box<dyn AsyncStream>,
    mut client_reader: StreamReader,
    client_hello_frame: Vec<u8>,
    initial_hmac: &ShadowTlsHmac,
    initial_xor_context: &ShadowTlsXorContext,
    remote_addr: NetLocation,
    client_connector: &TcpClientConnector,
    resolver: &Arc<dyn Resolver>,
) -> std::io::Result<ShadowTlsStream> {
    // there will not be any messages from a TLS server until we send ClientHello, so a noop stream
    // is fine.
    let mut noop_stream: Box<dyn AsyncStream> = Box::new(NoopStream);

    // this is confusing, but the TLS server is called client_stream.
    let mut client_stream = client_connector
        .connect(&mut noop_stream, remote_addr, resolver)
        .await
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("failed to connect to remote handshake server: {e}"),
            )
        })?;

    write_all(&mut client_stream, &client_hello_frame)
        .await
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("failed to send ClientHello to remote server: {e}"),
            )
        })?;
    client_stream.flush().await.map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            format!("failed to flush ClientHello to remote server: {e}"),
        )
    })?;

    let mut server_reader = StreamReader::new_with_buffer_size(TLS_FRAME_MAX_LEN);
    let server_header_bytes = server_reader
        .read_slice(&mut client_stream, TLS_HEADER_LEN)
        .await
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                format!("failed to read ServerHello header from remote server: {e}"),
            )
        })?;

    let server_payload_size = u16::from_be_bytes([server_header_bytes[3], server_header_bytes[4]]);

    let mut server_hello_frame =
        Vec::with_capacity(server_header_bytes.len() + server_payload_size as usize);
    server_hello_frame.extend_from_slice(server_header_bytes);

    let server_payload_bytes = server_reader
        .read_slice(&mut client_stream, server_payload_size as usize)
        .await
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                format!(
                    "failed to read ServerHello payload from remote server (size: {server_payload_size}): {e}"
                ),
            )
        })?;
    server_hello_frame.extend_from_slice(server_payload_bytes);

    let ParsedServerHello { server_random } =
        parse_server_hello(&server_hello_frame).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to parse ServerHello from remote server: {e}"),
            )
        })?;

    // write the server hello frame to the client
    write_all(&mut server_stream, &server_hello_frame)
        .await
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("failed to write ServerHello to client: {e}"),
            )
        })?;
    server_stream.flush().await.map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            format!("failed to flush ServerHello to client: {e}"),
        )
    })?;

    let mut hmac_server_random = initial_hmac.clone();
    hmac_server_random.update(&server_random);

    let mut hmac_client_data = hmac_server_random.clone();
    hmac_client_data.update(b"C");

    let mut hmac_server_data = hmac_server_random.clone();
    hmac_server_data.update(b"S");

    let server_app_data_xor = {
        let mut key_context = initial_xor_context.0.clone();
        key_context.update(&server_random);
        key_context.finish().as_ref().to_vec()
    };

    let mut server_frame = vec![];
    let mut client_frame = vec![];

    loop {
        tokio::select! {
            server_read_result = server_reader.read_slice(&mut client_stream, TLS_HEADER_LEN) => {
                server_frame.clear();

                let server_header_bytes = server_read_result
                    .map_err(|e| std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        format!("failed to read TLS header from remote server during handshake: {e}")
                    ))?;
                let server_payload_size = u16::from_be_bytes(server_header_bytes[3..5].try_into().unwrap()) as usize;
                server_frame.extend_from_slice(server_header_bytes);
                let server_payload_bytes = server_reader
                    .read_slice(&mut client_stream, server_payload_size)
                    .await
                    .map_err(|e| std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        format!("failed to read TLS payload from remote server during handshake (size {server_payload_size}): {e}")
                    ))?;
                server_frame.extend_from_slice(server_payload_bytes);

                let server_content_type = server_frame[0];
                if server_content_type == CONTENT_TYPE_APPLICATION_DATA {
                   if server_payload_size > TLS_FRAME_MAX_LEN - 4 {
                       return Err(std::io::Error::new(
                           std::io::ErrorKind::InvalidData,
                           "server payload too large to modify",
                       ));
                   }
                   // TODO: do this in a single loop, see the same comment in local handshake
                   let iter = server_frame[TLS_HEADER_LEN..TLS_HEADER_LEN + server_payload_size].iter_mut().zip(server_app_data_xor.iter().cycle());
                   for (byte, &key) in iter {
                       *byte ^= key;
                   }
                   server_frame.extend([0u8; 4]);
                   server_frame.copy_within(TLS_HEADER_LEN..TLS_HEADER_LEN + server_payload_size, TLS_HEADER_LEN + 4);

                   hmac_server_random.update(&server_frame[TLS_HEADER_LEN + 4..TLS_HEADER_LEN + 4 + server_payload_size]);
                   let hmac_digest = hmac_server_random.digest();
                   server_frame[TLS_HEADER_LEN..TLS_HEADER_LEN + 4]
                       .copy_from_slice(&hmac_digest);

                   let updated_payload_size = (server_payload_size as u16).wrapping_add(4);
                   server_frame[3..5].copy_from_slice(&updated_payload_size.to_be_bytes());
                }

                write_all(&mut server_stream, &server_frame).await
                    .map_err(|e| std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        format!("failed to write server frame to client: {e}")
                    ))?;
                server_stream.flush().await.map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        format!("failed to flush server frame to client: {e}"),
                    )
                })?;
            }
            client_read_result = client_reader.read_slice(&mut server_stream, TLS_HEADER_LEN) => {
                client_frame.clear();

                let client_header_bytes = client_read_result
                    .map_err(|e| std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        format!("failed to read TLS header from client during handshake: {e}")
                    ))?;

                let client_content_type = client_header_bytes[0];
                let client_payload_size = u16::from_be_bytes([client_header_bytes[3], client_header_bytes[4]]) as usize;
                client_frame.extend_from_slice(client_header_bytes);

                let client_payload_bytes = client_reader
                    .read_slice(&mut server_stream, client_payload_size)
                    .await
                    .map_err(|e| std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        format!("failed to read TLS payload from client during handshake (size {client_payload_size}): {e}")
                    ))?;

                if client_content_type == CONTENT_TYPE_APPLICATION_DATA {
                    let mut tmp_hmac = hmac_client_data.clone();
                    tmp_hmac.update(&client_payload_bytes[4..]);

                    if tmp_hmac.finalized_digest() == client_payload_bytes[..4] {
                        let initial_client_data = &client_payload_bytes[4..];

                        hmac_client_data.update(initial_client_data);
                        hmac_client_data.update(&hmac_client_data.digest());

                        let _ = client_stream.shutdown().await;

                        let mut shadow_tls_stream = ShadowTlsStream::new(
                            server_stream,
                            initial_client_data,
                            hmac_client_data,
                            hmac_server_data,
                            None,
                        ).map_err(|e| std::io::Error::other(
                            format!("failed to create ShadowTlsStream: {e}")
                        ))?;

                        let unparsed_data = client_reader.unparsed_data();
                        if !unparsed_data.is_empty() {
                            shadow_tls_stream.feed_initial_read_data(unparsed_data)
                                .map_err(|e| std::io::Error::other(
                                    format!("failed to feed initial data to ShadowTlsStream: {e}")
                                ))?;
                        }

                        return Ok(shadow_tls_stream);
                    }
                }

                client_frame.extend_from_slice(client_payload_bytes);
                write_all(&mut client_stream, &client_frame).await
                    .map_err(|e| std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        format!("failed to write client frame to remote server: {e}")
                    ))?;
                client_stream.flush().await.map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        format!("failed to flush client frame to remote server: {e}"),
                    )
                })?;
            }
        }
    }
}

#[inline]
async fn setup_local_handshake(
    mut server_stream: Box<dyn AsyncStream>,
    mut client_reader: StreamReader,
    client_hello_frame: Vec<u8>,
    initial_hmac: &ShadowTlsHmac,
    initial_xor_context: &ShadowTlsXorContext,
    server_config: Arc<rustls::ServerConfig>,
) -> std::io::Result<ShadowTlsStream> {
    let mut server_connection = rustls::ServerConnection::new(server_config).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to create server connection: {e}"),
        )
    })?;

    feed_server_connection(&mut server_connection, &client_hello_frame)?;

    server_connection.process_new_packets().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to process ClientHello packet: {e}"),
        )
    })?;

    if !server_connection.wants_write() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server connection wants no write",
        ));
    }

    // enough for full tls frame with header + frame
    let mut server_data = allocate_vec(TLS_FRAME_MAX_LEN);

    let server_data_len = read_server_connection(&mut server_connection, &mut server_data)?;

    if server_data_len < TLS_HEADER_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server data too short for header",
        ));
    }

    let server_hello_payload_size = u16::from_be_bytes([server_data[3], server_data[4]]) as usize;
    if server_data_len < TLS_HEADER_LEN + server_hello_payload_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server data too short for payload",
        ));
    }

    let server_hello_frame = &server_data[0..TLS_HEADER_LEN + server_hello_payload_size];

    let ParsedServerHello { server_random } = parse_server_hello(server_hello_frame)?;

    // write the server hello frame to the client
    write_all(&mut server_stream, server_hello_frame).await?;

    // the server sends multiple frames after ServerHello, make sure we process the remaining
    // data
    let remaining_server_data_len =
        server_data_len - TLS_HEADER_LEN - server_hello_payload_size as usize;
    if remaining_server_data_len > 0 {
        server_data.copy_within(
            TLS_HEADER_LEN + server_hello_payload_size
                ..TLS_HEADER_LEN + server_hello_payload_size + remaining_server_data_len,
            0,
        );
    }
    let mut server_data_end_index = remaining_server_data_len;

    let mut hmac_server_random = initial_hmac.clone();
    hmac_server_random.update(&server_random);

    let mut hmac_client_data = hmac_server_random.clone();
    hmac_client_data.update(b"C");

    let mut hmac_server_data = hmac_server_random.clone();
    hmac_server_data.update(b"S");

    let server_app_data_xor = {
        let mut key_context = initial_xor_context.0.clone();
        key_context.update(&server_random);
        key_context.finish().as_ref().to_vec()
    };

    // copy bidirectionally until we find a matching hmac at the front of
    // an application data frame
    loop {
        // server write loop
        loop {
            if server_data_end_index < TLS_HEADER_LEN {
                if server_connection.wants_write() {
                    let server_data_len = read_server_connection_once(
                        &mut server_connection,
                        &mut server_data[server_data_end_index..],
                    )?;
                    server_data_end_index += server_data_len;
                    continue;
                }
                break;
            }

            let server_content_type = server_data[0];
            let server_legacy_version_major = server_data[1];
            let server_legacy_version_minor = server_data[2];
            if server_legacy_version_major != 3 || server_legacy_version_minor != 3 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "unexpected local server TLS version {server_legacy_version_major}.{server_legacy_version_minor}"
                    ),
                ));
            }
            let server_payload_size = u16::from_be_bytes([server_data[3], server_data[4]]) as usize;

            if server_data_end_index < TLS_HEADER_LEN + server_payload_size {
                // not enough for a complete frame.
                if server_connection.wants_write() {
                    let server_data_len = read_server_connection_once(
                        &mut server_connection,
                        &mut server_data[server_data_end_index..],
                    )?;
                    server_data_end_index += server_data_len;
                    continue;
                }
                break;
            }

            if server_content_type == CONTENT_TYPE_APPLICATION_DATA {
                // make sure there's enough space for the digest
                if server_payload_size > TLS_FRAME_MAX_LEN - 4 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "server payload too large to modify",
                    ));
                }
                // we need to modify the frame and push all the following frames back by 4
                // bytes as well
                if server_data_end_index > TLS_FRAME_MAX_LEN + TLS_HEADER_LEN - 4 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "server data too large to modify",
                    ));
                }

                // TODO: we could possibly do this in a single loop by starting from the end of the payload to the
                // beginning, and placing the xor'ed byte at its initial position + 4 for the
                // hash length. but we'd have to figure out which byte in `xor` it corresponds
                // to.
                let iter = server_data[TLS_HEADER_LEN..TLS_HEADER_LEN + server_payload_size]
                    .iter_mut()
                    .zip(server_app_data_xor.iter().cycle());
                for (byte, &key) in iter {
                    *byte ^= key;
                }

                // make space for the hmac digest
                server_data.copy_within(TLS_HEADER_LEN..server_data_end_index, TLS_HEADER_LEN + 4);
                server_data_end_index += 4;

                // calculate the digest and place it at the front of the payload
                hmac_server_random.update(
                    &server_data[TLS_HEADER_LEN + 4..TLS_HEADER_LEN + 4 + server_payload_size],
                );
                server_data[TLS_HEADER_LEN..TLS_HEADER_LEN + 4]
                    .copy_from_slice(&hmac_server_random.digest());

                // update the payload size
                let updated_payload_size = (server_payload_size as u16).wrapping_add(4);
                server_data[3..5].copy_from_slice(&updated_payload_size.to_be_bytes());

                write_all(&mut server_stream, &server_data[0..9 + server_payload_size]).await?;

                server_data.copy_within(9 + server_payload_size..server_data_end_index, 0);
                server_data_end_index -= 9 + server_payload_size;
            } else {
                write_all(
                    &mut server_stream,
                    &server_data[0..TLS_HEADER_LEN + server_payload_size],
                )
                .await?;

                server_data.copy_within(
                    TLS_HEADER_LEN + server_payload_size..server_data_end_index,
                    0,
                );
                server_data_end_index -= TLS_HEADER_LEN + server_payload_size;
            };
        }

        let client_header_bytes = client_reader
            .read_slice(&mut server_stream, 5)
            .await?
            .to_vec();
        let client_content_type = client_header_bytes[0];
        let _client_legacy_version_major = client_header_bytes[1];
        let _client_legacy_version_minor = client_header_bytes[2];
        let client_payload_size =
            u16::from_be_bytes([client_header_bytes[3], client_header_bytes[4]]);

        let client_payload_bytes = client_reader
            .read_slice(&mut server_stream, client_payload_size as usize)
            .await?;

        if client_content_type == CONTENT_TYPE_APPLICATION_DATA {
            let mut tmp_hmac = hmac_client_data.clone();
            tmp_hmac.update(&client_payload_bytes[4..]);

            if tmp_hmac.finalized_digest() == client_payload_bytes[..4] {
                let initial_client_data = &client_payload_bytes[4..];

                hmac_client_data.update(initial_client_data);
                hmac_client_data.update(&hmac_client_data.digest());

                let shadow_tls_stream = ShadowTlsStream::new(
                    server_stream,
                    initial_client_data,
                    hmac_client_data,
                    hmac_server_data,
                    None,
                )?;

                return Ok(shadow_tls_stream);
            }
        }

        feed_server_connection(&mut server_connection, &client_header_bytes)?;
        feed_server_connection(&mut server_connection, client_payload_bytes)?;

        server_connection.process_new_packets().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to process pre-auth client packets: {e}"),
            )
        })?;
    }
}

#[inline]
pub fn feed_server_connection(
    server_connection: &mut rustls::ServerConnection,
    data: &[u8],
) -> std::io::Result<()> {
    let mut cursor = Cursor::new(data);
    let mut i = 0;
    while i < data.len() {
        let n = server_connection.read_tls(&mut cursor).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to feed server connection: {e}"),
            )
        })?;
        i += n;
    }
    Ok(())
}

#[inline]
fn read_server_connection(
    server_connection: &mut rustls::ServerConnection,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    let mut server_data_cursor = Cursor::new(buf);
    while server_connection.wants_write() {
        server_connection
            .write_tls(&mut server_data_cursor)
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("failed to write tls frame: {e}"),
                )
            })?;
    }
    Ok(server_data_cursor.position() as usize)
}

#[inline]
fn read_server_connection_once(
    server_connection: &mut rustls::ServerConnection,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    let mut server_data_cursor = Cursor::new(buf);
    server_connection
        .write_tls(&mut server_data_cursor)
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to write tls frame: {e}"),
            )
        })?;
    Ok(server_data_cursor.position() as usize)
}
