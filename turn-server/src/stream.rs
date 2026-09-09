use async_trait::async_trait;
use std::{
    any::Any,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use util::Conn;

trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}

pub struct StreamConn {
    reader: Mutex<ReadHalf<Box<dyn IoStream>>>,
    writer: Mutex<WriteHalf<Box<dyn IoStream>>>,
    local: SocketAddr,
    remote: SocketAddr,
    idle_timeout: Duration,
    received_packet: AtomicBool,
    pub closed: CancellationToken,
}

impl StreamConn {
    pub fn new<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        stream: S,
        local: SocketAddr,
        remote: SocketAddr,
        idle_timeout: Duration,
    ) -> Self {
        let (reader, writer) = tokio::io::split(Box::new(stream) as Box<dyn IoStream>);
        Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            local,
            remote,
            idle_timeout,
            received_packet: AtomicBool::new(false),
            closed: CancellationToken::new(),
        }
    }
}

async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;
    let size = u16::from_be_bytes([header[2], header[3]]) as usize;
    let (length, padding) = match header[0] & 0xc0 {
        0x00 if size.is_multiple_of(4) => (20 + size, 0),
        0x40 => (4 + size, (4 - size % 4) % 4),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid TURN stream frame",
            ))
        }
    };
    if length > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TURN packet exceeds buffer",
        ));
    }
    buffer[..4].copy_from_slice(&header);
    reader.read_exact(&mut buffer[4..length]).await?;
    if padding > 0 {
        reader.read_exact(&mut [0u8; 3][..padding]).await?;
    }
    Ok(length)
}

#[async_trait]
impl Conn for StreamConn {
    async fn connect(&self, addr: SocketAddr) -> util::Result<()> {
        if addr != self.remote {
            return Err(io::Error::other("stream peer cannot change").into());
        }
        Ok(())
    }

    async fn recv(&self, buffer: &mut [u8]) -> util::Result<usize> {
        let wait = if self.received_packet.load(Ordering::Relaxed) {
            self.idle_timeout
        } else {
            self.idle_timeout.min(Duration::from_secs(10))
        };
        let result: io::Result<usize> = tokio::select! {
            _ = self.closed.cancelled() => Err(io::Error::new(io::ErrorKind::ConnectionAborted, "TURN stream closed")),
            result = tokio::time::timeout(wait, async {
                let mut reader = self.reader.lock().await;
                read_packet(&mut *reader, buffer).await
            }) => result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "TURN stream read timed out"))),
        };
        if result.is_err() {
            self.closed.cancel();
        } else {
            self.received_packet.store(true, Ordering::Relaxed);
        }
        result.map_err(Into::into)
    }

    async fn recv_from(&self, buffer: &mut [u8]) -> util::Result<(usize, SocketAddr)> {
        Ok((self.recv(buffer).await?, self.remote))
    }

    async fn send(&self, buffer: &[u8]) -> util::Result<usize> {
        let result: io::Result<()> = tokio::select! {
            _ = self.closed.cancelled() => Err(io::Error::new(io::ErrorKind::ConnectionAborted, "TURN stream closed")),
            result = tokio::time::timeout(Duration::from_secs(15), async {
                let mut writer = self.writer.lock().await;
                writer.write_all(buffer).await?;
                if buffer.len() >= 4 && buffer[0] & 0xc0 == 0x40 {
                    let size = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
                    let padded = (4 + size).div_ceil(4) * 4;
                    if padded > buffer.len() { writer.write_all(&[0u8; 3][..padded - buffer.len()]).await?; }
                }
                writer.flush().await
            }) => result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "TURN stream write timed out"))),
        };
        if result.is_err() {
            self.closed.cancel();
        }
        result?;
        Ok(buffer.len())
    }

    async fn send_to(&self, buffer: &[u8], target: SocketAddr) -> util::Result<usize> {
        if target != self.remote {
            return Err(io::Error::other("wrong TURN stream peer").into());
        }
        self.send(buffer).await
    }
    fn local_addr(&self) -> util::Result<SocketAddr> {
        Ok(self.local)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.remote)
    }
    async fn close(&self) -> util::Result<()> {
        self.closed.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            self.writer.lock().await.shutdown().await
        })
        .await;
        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

pub struct Prefixed<S> {
    pub first: Option<u8>,
    pub stream: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() > 0 {
            if let Some(first) = self.first.take() {
                buffer.put_slice(&[first]);
                return Poll::Ready(Ok(()));
            }
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_split_stun_and_padded_channel_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let first = vec![0x40, 0x01, 0, 3, 1, 2, 3, 0];
        let mut second = vec![0; 20];
        second[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        let expected = second.clone();
        tokio::spawn(async move {
            for byte in first.into_iter().chain(second) {
                writer.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let mut buffer = [0u8; 128];
        assert_eq!(read_packet(&mut reader, &mut buffer).await.unwrap(), 7);
        assert_eq!(&buffer[..7], &[0x40, 1, 0, 3, 1, 2, 3]);
        assert_eq!(read_packet(&mut reader, &mut buffer).await.unwrap(), 20);
        assert_eq!(&buffer[..20], expected);
    }

    #[tokio::test]
    async fn rejects_oversized_stream_frame() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_all(&[0x40, 0, 0xff, 0xff]).await.unwrap();
        assert!(read_packet(&mut reader, &mut [0u8; 1500]).await.is_err());
    }

    #[tokio::test]
    async fn releases_idle_streams_and_interrupts_partial_reads_on_close() {
        let (_remote, stream) = tokio::io::duplex(128);
        let address = "127.0.0.1:3478".parse().unwrap();
        let conn = StreamConn::new(stream, address, address, Duration::from_millis(20));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), conn.recv(&mut [0u8; 128]))
                .await
                .unwrap()
                .is_err()
        );
        assert!(conn.closed.is_cancelled());

        let (mut remote, stream) = tokio::io::duplex(128);
        remote.write_all(&[0x40, 0]).await.unwrap();
        let conn = std::sync::Arc::new(StreamConn::new(
            stream,
            address,
            address,
            Duration::from_secs(60),
        ));
        let reader = conn.clone();
        let pending = tokio::spawn(async move { reader.recv(&mut [0u8; 128]).await });
        tokio::task::yield_now().await;
        conn.close().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err());
    }
}
