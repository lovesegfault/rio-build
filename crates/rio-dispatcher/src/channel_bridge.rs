// Bridge between russh channels and AsyncRead/AsyncWrite streams using tokio utilities

use bytes::Bytes;
use futures_util::SinkExt;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::{CopyToBytes, SinkWriter, StreamReader};
use tokio_util::sync::PollSender;

/// Type alias for a channel-based AsyncRead
/// Uses tokio-stream's ReceiverStream + tokio-util's StreamReader
pub type ChannelReader = StreamReader<ReceiverStream<Result<Bytes, io::Error>>, Bytes>;

/// Create an AsyncRead from an mpsc::Receiver
#[allow(dead_code)]
pub fn channel_reader(rx: mpsc::Receiver<Result<Bytes, io::Error>>) -> ChannelReader {
    let stream = ReceiverStream::new(rx);
    StreamReader::new(stream)
}

// Type alias to reduce complexity
type ErrorMappedSink = futures_util::sink::SinkMapErr<
    PollSender<Bytes>,
    fn(tokio_util::sync::PollSendError<Bytes>) -> io::Error,
>;

/// Wrapper for SinkWriter that implements Debug
#[allow(dead_code)]
pub struct DebugSinkWriter {
    inner: SinkWriter<CopyToBytes<ErrorMappedSink>>,
}

impl fmt::Debug for DebugSinkWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DebugSinkWriter").finish()
    }
}

impl AsyncWrite for DebugSinkWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Create an AsyncWrite from an mpsc::Sender
#[allow(dead_code)]
pub fn channel_writer(tx: mpsc::Sender<Bytes>) -> DebugSinkWriter {
    fn map_err(_: tokio_util::sync::PollSendError<Bytes>) -> io::Error {
        io::Error::from(io::ErrorKind::BrokenPipe)
    }

    let poll_sender = PollSender::new(tx);
    let sink = poll_sender.sink_map_err(map_err as fn(_) -> _);
    let copy_to_bytes = CopyToBytes::new(sink);
    let writer = SinkWriter::new(copy_to_bytes);

    DebugSinkWriter { inner: writer }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_channel_reader_from_stream() {
        let (tx, rx) = mpsc::channel(10);

        // Send some data
        tx.send(Ok(Bytes::from("Hello, "))).await.unwrap();
        tx.send(Ok(Bytes::from("world!"))).await.unwrap();
        drop(tx);

        // Read using StreamReader
        let mut reader = channel_reader(rx);
        let mut buf = String::new();
        reader.read_to_string(&mut buf).await.unwrap();

        assert_eq!(buf, "Hello, world!");
    }

    #[tokio::test]
    async fn test_channel_writer() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut writer = channel_writer(tx);

        // Write some data
        writer.write_all(b"Test data").await.unwrap();
        writer.flush().await.unwrap();

        // Receive it from the channel
        let received = rx.recv().await.unwrap();
        assert_eq!(received, Bytes::from("Test data"));
    }

    #[tokio::test]
    async fn test_reader_chunked_data() {
        let (tx, rx) = mpsc::channel(10);

        // Send data in chunks
        tx.send(Ok(Bytes::from("chunk1"))).await.unwrap();
        tx.send(Ok(Bytes::from("chunk2"))).await.unwrap();
        tx.send(Ok(Bytes::from("chunk3"))).await.unwrap();
        drop(tx);

        let mut reader = channel_reader(rx);
        let mut buf = String::new();
        reader.read_to_string(&mut buf).await.unwrap();

        assert_eq!(buf, "chunk1chunk2chunk3");
    }

    #[tokio::test]
    async fn test_reader_with_error() {
        let (tx, rx) = mpsc::channel(10);

        tx.send(Ok(Bytes::from("good data"))).await.unwrap();
        tx.send(Err(io::Error::other("test error"))).await.unwrap();
        drop(tx);

        let mut reader = channel_reader(rx);
        let mut buf = Vec::new();
        let result = reader.read_to_end(&mut buf).await;

        // Should get the error
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
    }

    #[tokio::test]
    async fn test_writer_multiple_writes() {
        let (tx, mut rx) = mpsc::channel(10);
        let mut writer = channel_writer(tx);

        // Write multiple chunks
        writer.write_all(b"first ").await.unwrap();
        writer.write_all(b"second ").await.unwrap();
        writer.write_all(b"third").await.unwrap();
        writer.flush().await.unwrap();
        drop(writer);

        // Collect all data
        let mut all_data = Vec::new();
        while let Some(chunk) = rx.recv().await {
            all_data.extend_from_slice(&chunk);
        }

        assert_eq!(all_data, b"first second third");
    }
}
