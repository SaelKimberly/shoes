use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;

use crate::address::NetLocation;
use crate::async_stream::{AsyncStream, AsyncTargetedMessageStream};
use crate::client_proxy_selector::ClientProxySelector;
use crate::option_util::NoneOrOne;
use crate::tcp::tcp_client_connector::TcpClientConnector;

#[cfg(any(feature = "vmess", feature = "vless"))]
use crate::async_stream::AsyncMessageStream;

pub(crate) enum TcpServerSetupResult {
    TcpForward {
        remote_location: NetLocation,
        stream: Box<dyn AsyncStream>,
        need_initial_flush: bool,
        // the response to write to the server stream after a connection to the remote location is
        // successful
        connection_success_response: Option<Box<[u8]>>,
        // initial data to send to the remote location.
        initial_remote_data: Option<Box<[u8]>>,
        override_proxy_provider: NoneOrOne<Arc<ClientProxySelector<TcpClientConnector>>>,
    },
    #[cfg(any(feature = "vmess", feature = "vless"))]
    // TODO: support udp client proxy selector
    BidirectionalUdp {
        need_initial_flush: bool,
        remote_location: NetLocation,
        stream: Box<dyn AsyncMessageStream>,
        override_proxy_provider: NoneOrOne<Arc<ClientProxySelector<TcpClientConnector>>>,
    },
    MultiDirectionalUdp {
        need_initial_flush: bool,
        stream: Box<dyn AsyncTargetedMessageStream>,
        override_proxy_provider: NoneOrOne<Arc<ClientProxySelector<TcpClientConnector>>>,
        num_sockets: usize,
    },
}

impl TcpServerSetupResult {
    pub(crate) fn set_need_initial_flush(&mut self, need_initial_flush: bool) {
        #[cfg(any(feature = "vmess", feature = "vless"))]
        match self {
            TcpServerSetupResult::TcpForward {
                need_initial_flush: flush,
                ..
            }
            | TcpServerSetupResult::BidirectionalUdp {
                need_initial_flush: flush,
                ..
            }
            | TcpServerSetupResult::MultiDirectionalUdp {
                need_initial_flush: flush,
                ..
            } => {
                *flush = need_initial_flush;
            }
        }
        #[cfg(not(any(feature = "vmess", feature = "vless")))]
        match self {
            TcpServerSetupResult::TcpForward {
                need_initial_flush: flush,
                ..
            }
            | TcpServerSetupResult::MultiDirectionalUdp {
                need_initial_flush: flush,
                ..
            } => {
                *flush = need_initial_flush;
            }
        }
    }
    pub(crate) fn override_proxy_provider_unspecified(&self) -> bool {
        #[cfg(any(feature = "vmess", feature = "vless"))]
        match self {
            TcpServerSetupResult::TcpForward {
                override_proxy_provider,
                ..
            }
            | TcpServerSetupResult::BidirectionalUdp {
                override_proxy_provider,
                ..
            }
            | TcpServerSetupResult::MultiDirectionalUdp {
                override_proxy_provider,
                ..
            } => override_proxy_provider.is_unspecified(),
        }
        #[cfg(not(any(feature = "vmess", feature = "vless")))]
        match self {
            TcpServerSetupResult::TcpForward {
                override_proxy_provider,
                ..
            }
            | TcpServerSetupResult::MultiDirectionalUdp {
                override_proxy_provider,
                ..
            } => override_proxy_provider.is_unspecified(),
        }
    }

    pub(crate) fn set_override_proxy_provider(
        &mut self,
        override_proxy_provider: NoneOrOne<Arc<ClientProxySelector<TcpClientConnector>>>,
    ) {
        #[cfg(any(feature = "vmess", feature = "vless"))]
        match self {
            TcpServerSetupResult::TcpForward {
                override_proxy_provider: provider,
                ..
            }
            | TcpServerSetupResult::BidirectionalUdp {
                override_proxy_provider: provider,
                ..
            }
            | TcpServerSetupResult::MultiDirectionalUdp {
                override_proxy_provider: provider,
                ..
            } => {
                *provider = override_proxy_provider;
            }
        }
        #[cfg(not(any(feature = "vmess", feature = "vless")))]
        match self {
            TcpServerSetupResult::TcpForward {
                override_proxy_provider: provider,
                ..
            }
            | TcpServerSetupResult::MultiDirectionalUdp {
                override_proxy_provider: provider,
                ..
            } => {
                *provider = override_proxy_provider;
            }
        }
    }
}

#[async_trait]
pub(crate) trait TcpServerHandler: Send + Sync + Debug {
    async fn setup_server_stream(
        &self,
        server_stream: Box<dyn AsyncStream>,
    ) -> std::io::Result<TcpServerSetupResult>;
}

pub(crate) struct TcpClientSetupResult {
    pub(crate) client_stream: Box<dyn AsyncStream>,
}

#[async_trait]
pub(crate) trait TcpClientHandler: Send + Sync + Debug {
    async fn setup_client_stream(
        &self,
        server_stream: &mut Box<dyn AsyncStream>,
        client_stream: Box<dyn AsyncStream>,
        remote_location: NetLocation,
    ) -> std::io::Result<TcpClientSetupResult>;
}
