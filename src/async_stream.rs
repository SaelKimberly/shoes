use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UdpSocket};

#[cfg(target_family = "unix")]
use tokio::net::UnixStream;

use crate::address::NetLocation;

pub(crate) trait AsyncPing {
    fn supports_ping(&self) -> bool;

    // Write a ping message to the stream, if supported.
    // This should end up calling the highest level stream abstraction that supports
    // pings, and should only result in a single message.
    fn poll_write_ping(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<bool>>;
}

pub(crate) trait AsyncReadMessage {
    fn poll_read_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>>;
}

pub(crate) trait AsyncWriteMessage {
    fn poll_write_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<()>>;
}

pub(crate) trait AsyncFlushMessage {
    fn poll_flush_message(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>>;
}

pub(crate) trait AsyncShutdownMessage {
    fn poll_shutdown_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>>;
}

pub(crate) trait AsyncReadTargetedMessage {
    fn poll_read_targeted_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<NetLocation>>;
}

pub(crate) trait AsyncWriteTargetedMessage {
    fn poll_write_targeted_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: &NetLocation,
    ) -> Poll<std::io::Result<()>>;
}

pub(crate) trait AsyncReadSourcedMessage {
    fn poll_read_sourced_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<SocketAddr>>;
}

pub(crate) trait AsyncWriteSourcedMessage {
    fn poll_write_sourced_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        source: &SocketAddr,
    ) -> Poll<std::io::Result<()>>;
}

impl AsyncReadMessage for UdpSocket {
    fn poll_read_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.poll_recv(cx, buf)
    }
}

impl AsyncWriteMessage for UdpSocket {
    fn poll_write_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<()>> {
        // TODO: send back an error if the whole buf.len() wasn't sent?
        self.poll_send(cx, buf).map(|result| result.map(|_| ()))
    }
}

impl AsyncFlushMessage for UdpSocket {
    fn poll_flush_message(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncShutdownMessage for UdpSocket {
    fn poll_shutdown_message(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub(crate) trait AsyncStream: AsyncRead + AsyncWrite + AsyncPing + Unpin + Send {}

pub(crate) trait AsyncMessageStream:
    AsyncReadMessage
    + AsyncWriteMessage
    + AsyncFlushMessage
    + AsyncShutdownMessage
    + AsyncPing
    + Unpin
    + Send
{
}

/// Server stream trait connected to proxy clients, where received messages have a target address,
/// and we write forwarded messages along with the source address we received them from.
pub(crate) trait AsyncTargetedMessageStream:
    AsyncReadTargetedMessage
    + AsyncWriteSourcedMessage
    + AsyncFlushMessage
    + AsyncShutdownMessage
    + AsyncPing
    + Unpin
    + Send
{
}

/// Client stream trait connected directly to targets or to proxy servers, where received messages
/// come with a source address, and we write where we want messages to be sent.
pub(crate) trait AsyncSourcedMessageStream:
    AsyncReadSourcedMessage
    + AsyncWriteTargetedMessage
    + AsyncFlushMessage
    + AsyncShutdownMessage
    + AsyncPing
    + Unpin
    + Send
{
}

impl AsyncPing for TcpStream {
    fn supports_ping(&self) -> bool {
        false
    }

    fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        unimplemented!();
    }
}

impl AsyncStream for TcpStream {}

#[cfg(target_family = "unix")]
impl AsyncPing for UnixStream {
    fn supports_ping(&self) -> bool {
        false
    }

    fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        unimplemented!();
    }
}

#[cfg(target_family = "unix")]
impl AsyncStream for UnixStream {}

impl AsyncPing for UdpSocket {
    fn supports_ping(&self) -> bool {
        false
    }

    fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        unimplemented!();
    }
}

impl AsyncMessageStream for UdpSocket {}

impl<AS> AsyncPing for tokio_rustls::client::TlsStream<AS>
where
    AS: AsyncPing + Unpin,
{
    fn supports_ping(&self) -> bool {
        self.get_ref().0.supports_ping()
    }

    fn poll_write_ping(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        let this = self.get_mut();
        Pin::new(this.get_mut().0).poll_write_ping(cx)
    }
}

impl<AS> AsyncStream for tokio_rustls::client::TlsStream<AS> where AS: AsyncStream {}

impl<AS> AsyncPing for tokio_rustls::server::TlsStream<AS>
where
    AS: AsyncPing + Unpin,
{
    fn supports_ping(&self) -> bool {
        self.get_ref().0.supports_ping()
    }

    fn poll_write_ping(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        let this = self.get_mut();
        Pin::new(this.get_mut().0).poll_write_ping(cx)
    }
}

impl<AS> AsyncStream for tokio_rustls::server::TlsStream<AS> where AS: AsyncStream {}

// pattern copied from deref_async_read macro: https://docs.rs/tokio/latest/src/tokio/io/async_read.rs.html#60
impl<T: ?Sized + AsyncPing + Unpin> AsyncPing for Box<T> {
    fn supports_ping(&self) -> bool {
        (**self).supports_ping()
    }

    fn poll_write_ping(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<bool>> {
        Pin::new(&mut **self).poll_write_ping(cx)
    }
}

impl<T: ?Sized + AsyncPing + Unpin> AsyncPing for &mut T {
    fn supports_ping(&self) -> bool {
        (**self).supports_ping()
    }

    fn poll_write_ping(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<bool>> {
        Pin::new(&mut **self).poll_write_ping(cx)
    }
}

impl<T: ?Sized + AsyncReadMessage + Unpin> AsyncReadMessage for Box<T> {
    fn poll_read_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_read_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncReadMessage + Unpin> AsyncReadMessage for &mut T {
    fn poll_read_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_read_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncWriteMessage + Unpin> AsyncWriteMessage for Box<T> {
    fn poll_write_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_write_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncWriteMessage + Unpin> AsyncWriteMessage for &mut T {
    fn poll_write_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_write_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncFlushMessage + Unpin> AsyncFlushMessage for Box<T> {
    fn poll_flush_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_flush_message(cx)
    }
}

impl<T: ?Sized + AsyncFlushMessage + Unpin> AsyncFlushMessage for &mut T {
    fn poll_flush_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_flush_message(cx)
    }
}

impl<T: ?Sized + AsyncShutdownMessage + Unpin> AsyncShutdownMessage for Box<T> {
    fn poll_shutdown_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_shutdown_message(cx)
    }
}

impl<T: ?Sized + AsyncShutdownMessage + Unpin> AsyncShutdownMessage for &mut T {
    fn poll_shutdown_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_shutdown_message(cx)
    }
}

impl<T: ?Sized + AsyncReadTargetedMessage + Unpin> AsyncReadTargetedMessage for Box<T> {
    fn poll_read_targeted_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<NetLocation>> {
        Pin::new(&mut **self).poll_read_targeted_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncReadTargetedMessage + Unpin> AsyncReadTargetedMessage for &mut T {
    fn poll_read_targeted_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<NetLocation>> {
        Pin::new(&mut **self).poll_read_targeted_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncWriteTargetedMessage + Unpin> AsyncWriteTargetedMessage for Box<T> {
    fn poll_write_targeted_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: &NetLocation,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_write_targeted_message(cx, buf, target)
    }
}

impl<T: ?Sized + AsyncWriteTargetedMessage + Unpin> AsyncWriteTargetedMessage for &mut T {
    fn poll_write_targeted_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: &NetLocation,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_write_targeted_message(cx, buf, target)
    }
}

impl<T: ?Sized + AsyncReadSourcedMessage + Unpin> AsyncReadSourcedMessage for Box<T> {
    fn poll_read_sourced_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<SocketAddr>> {
        Pin::new(&mut **self).poll_read_sourced_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncReadSourcedMessage + Unpin> AsyncReadSourcedMessage for &mut T {
    fn poll_read_sourced_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<SocketAddr>> {
        Pin::new(&mut **self).poll_read_sourced_message(cx, buf)
    }
}

impl<T: ?Sized + AsyncWriteSourcedMessage + Unpin> AsyncWriteSourcedMessage for Box<T> {
    fn poll_write_sourced_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        source: &SocketAddr,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_write_sourced_message(cx, buf, source)
    }
}

impl<T: ?Sized + AsyncWriteSourcedMessage + Unpin> AsyncWriteSourcedMessage for &mut T {
    fn poll_write_sourced_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        source: &SocketAddr,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_write_sourced_message(cx, buf, source)
    }
}

impl<T: ?Sized + AsyncStream + Unpin> AsyncStream for Box<T> {}
impl<T: ?Sized + AsyncStream + Unpin> AsyncStream for &mut T {}

impl<T: ?Sized + AsyncMessageStream + Unpin> AsyncMessageStream for Box<T> {}
impl<T: ?Sized + AsyncMessageStream + Unpin> AsyncMessageStream for &mut T {}

impl<T: ?Sized + AsyncTargetedMessageStream + Unpin> AsyncTargetedMessageStream for Box<T> {}
impl<T: ?Sized + AsyncTargetedMessageStream + Unpin> AsyncTargetedMessageStream for &mut T {}

impl<T: ?Sized + AsyncSourcedMessageStream + Unpin> AsyncSourcedMessageStream for Box<T> {}
impl<T: ?Sized + AsyncSourcedMessageStream + Unpin> AsyncSourcedMessageStream for &mut T {}
