#![cfg(feature = "compio-runtime")]

use std::io::{self, Cursor};
use std::time::Duration;

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::util::Splittable;
use compio::io::{AsyncRead, AsyncWrite};
use futures_channel::oneshot;
use sockudo_ws::{CompioWebSocketStream, Config, Message};

struct BlockedWriter(Option<oneshot::Sender<()>>);

impl AsyncWrite for BlockedWriter {
    async fn write<B: IoBuf>(&mut self, _buf: B) -> BufResult<usize, B> {
        self.0.take().unwrap().send(()).unwrap();
        std::future::pending().await
    }
    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BufferedInput {
    reader: Cursor<Vec<u8>>,
    writer: BlockedWriter,
}

impl AsyncRead for BufferedInput {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.reader.read(buf).await
    }
}

impl AsyncWrite for BufferedInput {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.writer.write(buf).await
    }
    async fn flush(&mut self) -> io::Result<()> {
        self.writer.flush().await
    }
    async fn shutdown(&mut self) -> io::Result<()> {
        self.writer.shutdown().await
    }
}

impl Splittable for BufferedInput {
    type ReadHalf = Cursor<Vec<u8>>;
    type WriteHalf = BlockedWriter;
    fn split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        (self.reader, self.writer)
    }
}

fn buffered_connection() -> (BufferedInput, oneshot::Receiver<()>) {
    let (entered, blocked) = oneshot::channel();
    let frames = (0..64u8).flat_map(|sequence| [0x82, 1, sequence]).collect();
    (
        BufferedInput {
            reader: Cursor::new(frames),
            writer: BlockedWriter(Some(entered)),
        },
        blocked,
    )
}

#[compio::test]
async fn ordinary_messages_pass_a_blocked_compio_writer() {
    let (io, blocked) = buffered_connection();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, Config::default()).split();
    let send =
        compio::runtime::spawn(
            async move { writer.send(Message::binary(b"out".as_slice())).await },
        );
    blocked.await.unwrap();
    for sequence in 0..64u8 {
        let message = compio::time::timeout(Duration::from_secs(1), reader.next())
            .await
            .expect("ordinary reads must not await the blocked writer")
            .unwrap()
            .unwrap();
        assert_eq!(message.as_bytes(), &[sequence]);
    }
    drop(reader);
    assert!(send.await.unwrap().is_err());
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_messages_pass_a_blocked_compio_writer() {
    let (io, blocked) = buffered_connection();
    let ws = sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        Config::default(),
        sockudo_ws::deflate::DeflateConfig::default(),
    );
    let (mut reader, mut writer) = ws.split();
    let send =
        compio::runtime::spawn(
            async move { writer.send(Message::binary(b"out".as_slice())).await },
        );
    blocked.await.unwrap();
    for sequence in 0..64u8 {
        let message = compio::time::timeout(Duration::from_secs(1), reader.next())
            .await
            .expect("ordinary reads must not await the blocked writer")
            .unwrap()
            .unwrap();
        assert_eq!(message.as_bytes(), &[sequence]);
    }
    drop(reader);
    assert!(send.await.unwrap().is_err());
}

#[compio::test]
async fn data_activity_defers_compio_idle_timeout() {
    use compio::io::AsyncWriteExt;
    use compio::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, _writer) = CompioWebSocketStream::client(stream, config).split();
    compio::time::sleep(Duration::from_millis(500)).await;
    peer.write_all(b"\x82\x01x".to_vec()).await.0.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"x");
    compio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !reader.is_closed(),
        "old timer must recheck coalesced activity"
    );
    assert!(matches!(
        compio::time::timeout(Duration::from_secs(2), reader.next())
            .await
            .unwrap(),
        Some(Err(sockudo_ws::Error::IdleTimeout))
    ));
}

#[compio::test]
async fn eof_after_buffered_data_terminates_compio_split_reads() {
    let (io, _blocked) = buffered_connection();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, mut writer) = CompioWebSocketStream::client(io, config).split();
    for sequence in 0..64u8 {
        assert_eq!(
            reader.next().await.unwrap().unwrap().as_bytes(),
            &[sequence]
        );
    }
    assert!(reader.next().await.is_none());
    assert!(reader.next().await.is_none());
    assert!(matches!(
        writer.send_text("closed").await,
        Err(sockudo_ws::Error::ConnectionClosed)
    ));
}

#[compio::test]
async fn buffered_compio_reads_yield_before_draining_a_large_batch() {
    let (io, _blocked) = buffered_connection();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, _writer) = CompioWebSocketStream::client(io, config).split();
    let mut drain = std::pin::pin!(async {
        for sequence in 0..64u8 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &[sequence]
            );
        }
    });
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    drain.await;
}

#[compio::test]
async fn cancelling_a_cooperative_read_preserves_the_next_message() {
    let (io, _blocked) = buffered_connection();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, _writer) = CompioWebSocketStream::client(io, config).split();
    for sequence in 0..64u8 {
        let observed = {
            let mut next = std::pin::pin!(reader.next());
            futures_util::poll!(next.as_mut())
        };
        match observed {
            std::task::Poll::Ready(Some(Ok(message))) => {
                assert_eq!(message.as_bytes(), &[sequence])
            }
            std::task::Poll::Pending => {
                assert_eq!(
                    reader.next().await.unwrap().unwrap().as_bytes(),
                    &[sequence]
                );
                return;
            }
            other => panic!("unexpected read: {other:?}"),
        }
    }
    panic!("the buffered batch must yield before it is exhausted");
}
