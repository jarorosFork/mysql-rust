//! Stream types supporting the mid-handshake TLS upgrade.
//!
//! MySQL's TLS is STARTTLS-style: the server sends its `HandshakeV10` in
//! plaintext, the client replies with an SSLRequest, and only *then* is the
//! socket upgraded to TLS (the real handshake response follows encrypted).
//! `ConnStream` lets a `Connection` hold either a plain or a TLS stream and
//! swap between them; `PrefixedStream` makes the upgrade correct when the
//! client has already pipelined TLS bytes onto the socket.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

/// A per-connection stream: plain TCP, or TLS after an upgrade.
pub(crate) enum ConnStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<PrefixedStream<TcpStream>>>),
    /// Transient placeholder held only for the instant of the TLS upgrade
    /// (so the plain `TcpStream` can be moved out). Never used for I/O.
    Upgrading,
}

fn upgrading_error() -> io::Error {
    io::Error::other("connection stream is mid-TLS-upgrade")
}

impl AsyncRead for ConnStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            ConnStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            ConnStream::Upgrading => Poll::Ready(Err(upgrading_error())),
        }
    }
}

impl AsyncWrite for ConnStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            ConnStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            ConnStream::Upgrading => Poll::Ready(Err(upgrading_error())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_flush(cx),
            ConnStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            ConnStream::Upgrading => Poll::Ready(Err(upgrading_error())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            ConnStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            ConnStream::Upgrading => Poll::Ready(Err(upgrading_error())),
        }
    }
}

/// A reader that yields an initial buffer of bytes before delegating to an
/// inner stream. Used so the TLS handshake sees any bytes that were already
/// read off the socket into the connection's buffer before the upgrade — if
/// the client pipelined its TLS ClientHello right after the SSLRequest, those
/// bytes would otherwise be stranded in the MySQL read buffer and lost.
pub(crate) struct PrefixedStream<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    pub(crate) fn new(prefix: Vec<u8>, inner: S) -> Self {
        PrefixedStream {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn prefixed_stream_yields_prefix_then_inner() {
        // Inner is a fixed byte source; prefix is prepended.
        let inner = std::io::Cursor::new(b"world".to_vec());
        let mut stream = PrefixedStream::new(b"hello ".to_vec(), inner);
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"hello world");
    }

    #[tokio::test]
    async fn prefixed_stream_with_empty_prefix_is_transparent() {
        let inner = std::io::Cursor::new(b"data".to_vec());
        let mut stream = PrefixedStream::new(Vec::new(), inner);
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"data");
    }
}
