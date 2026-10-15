#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use futures_util::FutureExt;
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

// Control-frame counts around and beyond the split control queue capacity, so
// that one count places the cancelled hand-off on the trailing Close or parse
// error.
const CONTROL_COUNTS: std::ops::RangeInclusive<usize> = 1..=64;
const PING: [u8; 2] = [0x89, 0x00];
const PONG: [u8; 2] = [0x8a, 0x00];
const CLOSE_1000: [u8; 4] = [0x88, 0x02, 0x03, 0xe8];
const RESERVED_OPCODE: [u8; 2] = [0x83, 0x00];

fn config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

fn burst(frame: [u8; 2], count: usize, tail: &[u8]) -> Vec<u8> {
    let mut wire = frame.repeat(count);
    wire.extend_from_slice(tail);
    wire
}

/// Reports whether the peer received a (masked) Close frame from the client.
async fn peer_received_close(peer: &mut DuplexStream) -> bool {
    let mut wire = Vec::new();
    let mut buf = [0; 4096];
    while let Ok(Ok(read)) =
        tokio::time::timeout(Duration::from_millis(200), peer.read(&mut buf)).await
    {
        if read == 0 {
            break;
        }
        wire.extend_from_slice(&buf[..read]);
    }
    let mut at = 0;
    while at + 2 <= wire.len() {
        if wire[at] & 0x0f == 0x08 {
            return true;
        }
        at += 2 + 4 + usize::from(wire[at + 1] & 0x7f);
    }
    false
}

// Polls next() without yielding, so the control driver cannot drain its queue,
// and drops each call that waits, as select! or timeout would. Every cancelled
// call is followed by one awaited call, which must neither hang nor skip input.
macro_rules! receive_with_cancellation {
    ($reader:expr) => {{
        let mut received = Vec::new();
        let mut cancelled = 0;
        loop {
            let item = match $reader.next().now_or_never() {
                Some(item) => item,
                None => {
                    cancelled += 1;
                    tokio::time::timeout(Duration::from_secs(5), $reader.next())
                        .await
                        .expect("next() must resume a cancelled control hand-off")
                }
            };
            let Some(item) = item else { break };
            let last = matches!(item, Ok(Message::Close(_)) | Err(_));
            received.push(item);
            if last {
                break;
            }
        }
        (received, cancelled)
    }};
}

fn controls(received: &[sockudo_ws::Result<Message>]) -> usize {
    received
        .iter()
        .filter(|item| matches!(item, Ok(Message::Ping(_) | Message::Pong(_))))
        .count()
}

macro_rules! handoff_cases {
    ($module:ident, $split:expr) => {
        mod $module {
            use super::*;

            #[tokio::test]
            async fn cancelled_handoff_keeps_controls_and_peer_close() {
                let mut cancelled_any = false;
                for frame in [PING, PONG] {
                    for count in CONTROL_COUNTS {
                        let (io, mut peer) = tokio::io::duplex(64 * 1024);
                        let (mut reader, _writer) = ($split)(io);
                        peer.write_all(&burst(frame, count, &CLOSE_1000)).await.unwrap();

                        let (received, cancelled) = receive_with_cancellation!(reader);

                        cancelled_any |= cancelled > 0;
                        assert_eq!(controls(&received), count, "{frame:x?} x {count}");
                        assert!(matches!(
                            received.last(),
                            Some(Ok(Message::Close(Some(reason)))) if reason.code == 1000
                        ));
                        assert_eq!(received.len(), count + 1, "{frame:x?} x {count}");
                        assert!(peer_received_close(&mut peer).await, "{frame:x?} x {count}");
                    }
                }
                assert!(cancelled_any, "the fixture must cancel a waiting hand-off");
            }

            #[tokio::test]
            async fn cancelled_handoff_keeps_parse_error() {
                let mut cancelled_any = false;
                for frame in [PING, PONG] {
                    for count in CONTROL_COUNTS {
                        let (io, mut peer) = tokio::io::duplex(64 * 1024);
                        let (mut reader, _writer) = ($split)(io);
                        peer.write_all(&burst(frame, count, &RESERVED_OPCODE)).await.unwrap();

                        let (received, cancelled) = receive_with_cancellation!(reader);

                        cancelled_any |= cancelled > 0;
                        assert_eq!(controls(&received), count, "{frame:x?} x {count}");
                        assert!(matches!(received.last(), Some(Err(_))));
                        assert_eq!(received.len(), count + 1, "{frame:x?} x {count}");
                        let after = tokio::time::timeout(Duration::from_secs(5), reader.next())
                            .await
                            .expect("the reader must end after reporting a parse error");
                        assert!(after.is_none());
                    }
                }
                assert!(cancelled_any, "the fixture must cancel a waiting hand-off");
            }

            #[tokio::test]
            async fn waiting_handoff_ends_when_the_driver_stops() {
                let (io, mut peer) = tokio::io::duplex(64 * 1024);
                let (mut reader, writer) = ($split)(io);
                peer.write_all(&burst(PING, *CONTROL_COUNTS.end(), &CLOSE_1000)).await.unwrap();
                while reader.next().now_or_never().is_some() {}

                // Dropping either half cancels the control driver.
                drop(writer);

                tokio::time::timeout(Duration::from_secs(5), async {
                    while reader.next().await.is_some() {}
                })
                .await
                .expect("a waiting hand-off must observe the stopped driver");
            }
        }
    };
}

handoff_cases!(plain, |io| WebSocketStream::client(io, config()).split());

#[cfg(feature = "permessage-deflate")]
handoff_cases!(compressed, |io| {
    sockudo_ws::CompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    )
    .split()
});
