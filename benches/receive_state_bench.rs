//! Controlled native-reader cost. No socket, TLS, peer task or production arrival claim.
use sockudo_ws::{Config, Http1, Message, SplitReader, WebSocketStream, stream::Stream};
use std::{
    collections::VecDeque,
    hint::black_box,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct Input {
    wire: Arc<Vec<u8>>,
    offset: usize,
    pending: bool,
    inject_pending: bool,
}

impl AsyncRead for Input {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let end = (self.offset + buf.remaining()).min(self.wire.len());
        buf.put_slice(&self.wire[self.offset..end]);
        self.offset = end;
        if end == self.wire.len() {
            self.offset = 0;
        }
        self.pending = self.inject_pending;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Input {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn measure<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    streams: Vec<WebSocketStream<S>>,
    payload: &[u8],
    retain: usize,
) {
    let (mut readers, _writers): (Vec<SplitReader<S>>, Vec<_>) =
        streams.into_iter().map(WebSocketStream::split).unzip();
    let mut retained: Vec<VecDeque<Message>> = (0..readers.len())
        .map(|_| VecDeque::with_capacity(retain + 1))
        .collect();
    // Warm every connection and verify the complete payload before timing.
    for _ in 0..512 {
        for (reader, held) in readers.iter_mut().zip(&mut retained) {
            let message = reader.next().await.unwrap().unwrap();
            assert_eq!(message.as_bytes(), payload);
            held.push_back(message);
            if held.len() > retain {
                held.pop_front();
            }
        }
    }
    let mut samples = Vec::with_capacity(64);
    for _ in 0..64 {
        let start = Instant::now();
        for _ in 0..256 {
            for (reader, held) in readers.iter_mut().zip(&mut retained) {
                let message = reader.next().await.unwrap().unwrap();
                black_box(message.as_bytes());
                held.push_back(message);
                if held.len() > retain {
                    held.pop_front();
                }
            }
        }
        samples.push(start.elapsed().as_nanos() as f64 / (256 * readers.len()) as f64);
    }
    // Check each reader after measurement even when retain=0 drops every sample.
    for (reader, held) in readers.iter_mut().zip(&retained) {
        assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), payload);
        for message in held {
            assert_eq!(message.as_bytes(), payload);
        }
    }
    println!("block,ns_per_message");
    for (block, value) in samples.into_iter().enumerate() {
        println!("{block},{value:.6}");
    }
}

fn main() {
    let mut args: Vec<_> = std::env::args().filter(|arg| arg != "--bench").collect();
    if args.len() == 1 {
        args.extend(["-", "1", "1", "ready", "0", "typed"].map(str::to_owned));
    }
    assert_eq!(
        args.len(),
        7,
        "fixture connections frames_per_read ready|pending retain typed|boxed"
    );
    let payload = if args[1] == "-" {
        br#"{"sequence":1,"value":42}"#.to_vec()
    } else {
        std::fs::read(&args[1]).unwrap()
    };
    std::str::from_utf8(&payload).unwrap();
    let connections: usize = args[2].parse().unwrap();
    let batch: usize = args[3].parse().unwrap();
    assert!(connections > 0 && batch > 0);
    let pending = match args[4].as_str() {
        "ready" => false,
        "pending" => true,
        _ => panic!("read state"),
    };
    let retain = args[5].parse().unwrap();
    let len = payload.len();
    let mut frame = vec![0x81];
    if len < 126 {
        frame.push(len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&payload);
    let wire = Arc::new(frame.repeat(batch));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let inputs = (0..connections).map(|_| Input {
            wire: wire.clone(),
            offset: 0,
            pending,
            inject_pending: pending,
        });
        match args[6].as_str() {
            "typed" => {
                measure(
                    inputs
                        .map(|io| WebSocketStream::client(io, Config::default()))
                        .collect(),
                    &payload,
                    retain,
                )
                .await
            }
            "boxed" => {
                measure(
                    inputs
                        .map(|io| {
                            WebSocketStream::client(Stream::<Http1>::new(io), Config::default())
                        })
                        .collect(),
                    &payload,
                    retain,
                )
                .await
            }
            _ => panic!("dispatch"),
        }
    });
}
