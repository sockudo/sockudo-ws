#![cfg(feature = "tokio-runtime")]

use futures_util::{Sink, SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::io::AsyncReadExt;

#[derive(Default)]
struct WriteState {
    allowance: usize,
    flush_ready: bool,
    written: Vec<u8>,
    flush_polls: usize,
}

struct ControlledIo(std::rc::Rc<std::cell::RefCell<WriteState>>);

impl tokio::io::AsyncRead for ControlledIo {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Pending
    }
}

impl tokio::io::AsyncWrite for ControlledIo {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut state = self.0.borrow_mut();
        if state.allowance == 0 {
            return std::task::Poll::Pending;
        }
        let count = state.allowance.min(buf.len());
        state.written.extend_from_slice(&buf[..count]);
        state.allowance -= count;
        std::task::Poll::Ready(Ok(count))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let mut state = self.0.borrow_mut();
        state.flush_polls += 1;
        if state.flush_ready {
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Pending
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

macro_rules! partial_drain_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let state = std::rc::Rc::new(std::cell::RefCell::new(WriteState::default()));
            let config = Config::builder().max_backpressure(10).build();
            let mut ws = ($make)(ControlledIo(state.clone()), config);
            ws.feed(Message::Ping(bytes::Bytes::from_static(&[1; 8])))
                .await
                .unwrap();
            let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());

            // Manually poll after granting capacity: this mock has no scheduler.
            state.borrow_mut().allowance = 1;
            assert!(std::pin::Pin::new(&mut ws).poll_ready(&mut cx).is_pending());
            assert_eq!(state.borrow().written.len(), 1);
            // Nine queued bytes are below the threshold, but the drain continues.
            assert!(std::pin::Pin::new(&mut ws).poll_ready(&mut cx).is_pending());
            state.borrow_mut().allowance = 9;
            assert!(std::pin::Pin::new(&mut ws).poll_ready(&mut cx).is_pending());
            assert_eq!(state.borrow().written, [0x89, 8, 1, 1, 1, 1, 1, 1, 1, 1]);
            // Empty encoded output still owes the transport its pending flush.
            assert!(std::pin::Pin::new(&mut ws).poll_ready(&mut cx).is_pending());
            state.borrow_mut().flush_ready = true;
            assert!(matches!(
                std::pin::Pin::new(&mut ws).poll_ready(&mut cx),
                std::task::Poll::Ready(Ok(()))
            ));
        }
    };
}

partial_drain_case!(
    partial_drain_waits_for_transport_flush,
    WebSocketStream::server
);
#[cfg(feature = "permessage-deflate")]
partial_drain_case!(
    compressed_partial_drain_waits_for_transport_flush,
    |io, config| {
        sockudo_ws::CompressedWebSocketStream::server(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
        )
    }
);

macro_rules! batching_drain_case {
    ($name:ident, $make:expr, $coalesce:expr, $payload_len:expr) => {
        #[tokio::test]
        async fn $name() {
            let state = std::rc::Rc::new(std::cell::RefCell::new(WriteState::default()));
            let mut ws = ($make)(
                ControlledIo(state.clone()),
                Config::builder()
                    .max_backpressure(usize::MAX)
                    .write_coalescing($coalesce)
                    .build(),
            );
            let payload = vec![7; $payload_len];
            let mut expected = bytes::BytesMut::new();
            sockudo_ws::frame::encode_frame(
                &mut expected,
                sockudo_ws::frame::OpCode::Binary,
                &payload,
                true,
                None,
            );
            ws.feed(Message::binary(payload)).await.unwrap();
            state.borrow_mut().allowance = 1;
            {
                let next = ws.feed(Message::text("not accepted"));
                let mut next = std::pin::pin!(next);
                assert!(futures_util::poll!(next.as_mut()).is_pending());
            }
            // Cancelling feed preserves the active drain even below high-water.
            let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(std::pin::Pin::new(&mut ws).poll_ready(&mut cx).is_pending());
            state.borrow_mut().allowance = expected.len() - 1;
            assert!(std::pin::Pin::new(&mut ws).poll_ready(&mut cx).is_pending());
            assert_eq!(ws.write_buffer_len(), 0);
            assert_eq!(state.borrow().written.as_slice(), expected.as_ref());
            state.borrow_mut().flush_ready = true;
            assert!(matches!(
                std::pin::Pin::new(&mut ws).poll_ready(&mut cx),
                std::task::Poll::Ready(Ok(()))
            ));
        }
    };
}
batching_drain_case!(
    high_water_drain_survives_cancel,
    WebSocketStream::server,
    true,
    65532
);
batching_drain_case!(
    disabled_batching_waits_for_flush,
    WebSocketStream::server,
    false,
    8
);
#[cfg(feature = "permessage-deflate")]
batching_drain_case!(
    compressed_high_water_drain_survives_cancel,
    |io, config| {
        sockudo_ws::CompressedWebSocketStream::server(
            io,
            config,
            sockudo_ws::DeflateConfig {
                compression_threshold: usize::MAX,
                ..Default::default()
            },
        )
    },
    true,
    65532
);
#[cfg(feature = "permessage-deflate")]
batching_drain_case!(
    compressed_disabled_batching_waits_for_flush,
    |io, config| {
        sockudo_ws::CompressedWebSocketStream::server(
            io,
            config,
            sockudo_ws::DeflateConfig {
                compression_threshold: usize::MAX,
                ..Default::default()
            },
        )
    },
    false,
    8
);

macro_rules! completed_drain_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let state = std::rc::Rc::new(std::cell::RefCell::new(WriteState::default()));
            let mut ws = ($make)(
                ControlledIo(state.clone()),
                Config::builder().max_backpressure(10).build(),
            );
            ws.feed(Message::Ping(bytes::Bytes::from_static(&[1; 8])))
                .await
                .unwrap();
            {
                let next = ws.feed(Message::Ping(bytes::Bytes::from_static(&[2; 8])));
                let mut next = std::pin::pin!(next);
                assert!(futures_util::poll!(next.as_mut()).is_pending());
            }
            state.borrow_mut().allowance = 10;
            state.borrow_mut().flush_ready = true;
            ws.flush().await.unwrap();
            let polls = state.borrow().flush_polls;
            state.borrow_mut().flush_ready = false;
            let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(matches!(
                std::pin::Pin::new(&mut ws).poll_ready(&mut cx),
                std::task::Poll::Ready(Ok(()))
            ));
            assert_eq!(state.borrow().flush_polls, polls);
            assert_eq!(state.borrow().written, [0x89, 8, 1, 1, 1, 1, 1, 1, 1, 1]);
        }
    };
}
completed_drain_case!(
    flush_completes_cancelled_readiness_drain,
    WebSocketStream::server
);
#[cfg(feature = "permessage-deflate")]
completed_drain_case!(
    compressed_flush_completes_cancelled_readiness_drain,
    |io, config| {
        sockudo_ws::CompressedWebSocketStream::server(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
        )
    }
);

macro_rules! bulk_readiness_case {
    ($name:ident, $make:expr, $forward:expr) => {
        #[tokio::test]
        async fn $name() {
            let state = std::rc::Rc::new(std::cell::RefCell::new(WriteState::default()));
            let mut ws = ($make)(
                ControlledIo(state.clone()),
                Config::builder().max_backpressure(10).build(),
            );
            let mut input = futures_util::stream::iter([
                Ok::<_, sockudo_ws::Error>(Message::Ping(bytes::Bytes::from_static(&[1; 8]))),
                Ok(Message::Ping(bytes::Bytes::from_static(&[2; 8]))),
            ]);
            {
                let transfer = async {
                    if $forward {
                        input.forward(&mut ws).await
                    } else {
                        ws.send_all(&mut input).await
                    }
                };
                let mut transfer = std::pin::pin!(transfer);
                assert!(futures_util::poll!(transfer.as_mut()).is_pending());
            }
            // The second message was not accepted while the first frame was blocked.
            assert_eq!(ws.write_buffer_len(), 10);
            state.borrow_mut().allowance = 10;
            state.borrow_mut().flush_ready = true;
            ws.flush().await.unwrap();
            assert_eq!(state.borrow().written, [0x89, 8, 1, 1, 1, 1, 1, 1, 1, 1]);
        }
    };
}
bulk_readiness_case!(send_all_respects_readiness, WebSocketStream::server, false);
bulk_readiness_case!(forward_respects_readiness, WebSocketStream::server, true);
#[cfg(feature = "permessage-deflate")]
bulk_readiness_case!(
    compressed_send_all_respects_readiness,
    |io, config| {
        sockudo_ws::CompressedWebSocketStream::server(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
        )
    },
    false
);
#[cfg(feature = "permessage-deflate")]
bulk_readiness_case!(
    compressed_forward_respects_readiness,
    |io, config| {
        sockudo_ws::CompressedWebSocketStream::server(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
        )
    },
    true
);

macro_rules! coalesced_readiness_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            use tokio::io::AsyncWriteExt;
            let (io, mut peer) = tokio::io::duplex(128);
            let mut ws = ($make)(io, Config::builder().max_backpressure(8).build());
            peer.write_all(&[0x82, 1, 1, 0x82, 1, 2]).await.unwrap();
            assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), &[1]);
            ws.feed(Message::Ping(bytes::Bytes::from_static(&[3; 2])))
                .await
                .unwrap();
            assert_eq!(ws.write_buffer_len(), 8);
            ws.feed(Message::Ping(bytes::Bytes::from_static(&[4; 2])))
                .await
                .unwrap();
            let mut wire = [0; 8];
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                peer.read_exact(&mut wire),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(&wire[..2], &[0x89, 0x82]);
            assert_eq!([wire[6] ^ wire[2], wire[7] ^ wire[3]], [3, 3]);
            assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), &[2]);
        }
    };
}
coalesced_readiness_case!(
    readiness_drains_during_coalesced_read_batch,
    WebSocketStream::client
);
#[cfg(feature = "permessage-deflate")]
coalesced_readiness_case!(
    compressed_readiness_drains_during_coalesced_read_batch,
    |io, config| {
        sockudo_ws::CompressedWebSocketStream::client(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
        )
    }
);

macro_rules! closing_readiness_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let (io, _peer) = tokio::io::duplex(64);
            let config = Config::builder().max_backpressure(0).build();
            let mut ws = ($make)(io, config);
            ws.close(1000, "").await.unwrap();
            assert!(matches!(
                ws.feed(Message::binary(vec![1])).await,
                Err(sockudo_ws::Error::ConnectionClosed)
            ));
        }
    };
}

closing_readiness_case!(local_close_rejects_readiness, WebSocketStream::server);
#[cfg(feature = "permessage-deflate")]
closing_readiness_case!(compressed_local_close_rejects_readiness, |io, config| {
    sockudo_ws::CompressedWebSocketStream::server(io, config, sockudo_ws::DeflateConfig::default())
});

macro_rules! queued_threshold_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            for threshold in [0, 10] {
                let (io, mut peer) = tokio::io::duplex(1);
                let config = Config::builder().max_backpressure(threshold).build();
                let mut ws = ($make)(io, config);
                // One 10-byte frame may reach or exceed the soft threshold.
                ws.feed(Message::Ping(bytes::Bytes::from_static(&[1; 8])))
                    .await
                    .unwrap();
                let next = ws.feed(Message::Ping(bytes::Bytes::from_static(&[2; 8])));
                let mut next = std::pin::pin!(next);
                assert!(futures_util::poll!(next.as_mut()).is_pending());
                let mut wire = [0; 10];
                let (sent, read) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::join!(next, peer.read_exact(&mut wire))
                })
                .await
                .unwrap();
                sent.unwrap();
                read.unwrap();
                assert_eq!(wire, [0x89, 8, 1, 1, 1, 1, 1, 1, 1, 1]);
            }
        }
    };
}
queued_threshold_case!(
    unified_waits_for_queued_bytes_to_drain,
    WebSocketStream::server
);
#[cfg(feature = "permessage-deflate")]
queued_threshold_case!(compressed_waits_for_queued_bytes_to_drain, |io, config| {
    sockudo_ws::CompressedWebSocketStream::server(io, config, sockudo_ws::DeflateConfig::default())
});

macro_rules! large_message_case {
    ($name:ident, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let (io, peer) = tokio::io::duplex(4096);
            let (mut writer, _guard) = ($make)(io);
            let mut reader = WebSocketStream::client(peer, Config::default());
            let payload = bytes::Bytes::from(vec![1; 2 * 1024 * 1024]);
            let send = async {
                writer.send(Message::Binary(payload.clone())).await.unwrap();
                writer.send(Message::binary(vec![2])).await.unwrap();
            };
            let receive = async {
                assert!(matches!(reader.next().await.unwrap().unwrap(), Message::Binary(data) if data == payload));
                assert!(matches!(reader.next().await.unwrap().unwrap(), Message::Binary(data) if data.as_ref() == [2]));
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(send, receive);
            }).await.unwrap();
        }
    };
}
large_message_case!(unified_preserves_default_large_message_sends, |io| (
    WebSocketStream::server(io, Config::default()),
    ()
));
large_message_case!(split_preserves_default_large_message_sends, |io| {
    let (reader, writer) = WebSocketStream::server(io, Config::default()).split();
    (writer, reader)
});
// Keep the encoded frame above the threshold when testing compressed streams.
#[cfg(feature = "permessage-deflate")]
large_message_case!(compressed_preserves_default_large_message_sends, |io| (
    sockudo_ws::CompressedWebSocketStream::server(
        io,
        Config::default(),
        sockudo_ws::DeflateConfig {
            compression_threshold: usize::MAX,
            ..Default::default()
        }
    ),
    ()
));
#[cfg(feature = "permessage-deflate")]
large_message_case!(
    compressed_split_preserves_default_large_message_sends,
    |io| {
        let (reader, writer) = sockudo_ws::CompressedWebSocketStream::server(
            io,
            Config::default(),
            sockudo_ws::DeflateConfig {
                compression_threshold: usize::MAX,
                ..Default::default()
            },
        )
        .split();
        (writer, reader)
    }
);
