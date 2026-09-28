use sockudo_ws::Config;

#[test]
fn configurations_validate_text_by_default() {
    assert!(Config::default().validate_text_utf8);
    assert!(Config::uws_defaults().validate_text_utf8);
    assert!(Config::builder().build().validate_text_utf8);
    assert!(
        !Config::builder()
            .validate_text_utf8(false)
            .build()
            .validate_text_utf8
    );
}

// All constructors consume the policy before the first leftover byte is parsed.
#[cfg(any(feature = "tokio-runtime", feature = "compio-runtime"))]
macro_rules! receive_case {
    ($name:ident, $wire:expr, $constructor:expr, $split:expr) => {
        #[rstest::rstest]
        #[case::validation_enabled(true)]
        #[case::validation_disabled(false)]
        fn $name(#[case] enabled: bool) {
            run(async {
                let (stream, _peer) = pair().await;
                let config = Config::builder()
                    .validate_text_utf8(enabled)
                    .auto_ping(false)
                    .idle_timeout(0)
                    .build();
                let wire = bytes::Bytes::from_static($wire);
                let ws = ($constructor)(stream, config, wire);
                let (mut reader, _writer) = ($split)(ws);
                let message = reader.next().await.unwrap();
                if enabled {
                    assert!(matches!(message, Err(sockudo_ws::Error::InvalidUtf8)));
                } else {
                    let message = message.unwrap();
                    assert!(message.is_text());
                    assert_eq!(message.as_bytes(), b"\xff");
                    assert!(message.as_text().is_none());
                }
            });
        }
    };
}

#[cfg(feature = "tokio-runtime")]
mod tokio_cases {
    use super::*;
    use futures_util::StreamExt;
    use sockudo_ws::{Role, WebSocketStream};

    fn run(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future);
    }
    async fn pair() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        tokio::io::duplex(1024)
    }
    receive_case!(
        plain_unified_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| WebSocketStream::from_raw_with_leftover(
            io,
            Role::Client,
            config,
            Some(wire)
        ),
        |ws| (ws, ())
    );
    receive_case!(
        plain_split_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| WebSocketStream::from_raw_with_leftover(
            io,
            Role::Client,
            config,
            Some(wire)
        ),
        |ws: WebSocketStream<_>| ws.split()
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_unified_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| sockudo_ws::CompressedWebSocketStream::client_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws| (ws, ())
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_split_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| sockudo_ws::CompressedWebSocketStream::client_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws: sockudo_ws::CompressedWebSocketStream<_>| ws.split()
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_server_unified_honors_text_policy,
        b"\x81\x81\x00\x00\x00\x00\xff",
        |io, config, wire| sockudo_ws::CompressedWebSocketStream::server_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws| (ws, ())
    );

    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_server_split_honors_text_policy,
        b"\x81\x81\x00\x00\x00\x00\xff",
        |io, config, wire| sockudo_ws::CompressedWebSocketStream::server_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws: sockudo_ws::CompressedWebSocketStream<_>| ws.split()
    );
}

#[cfg(feature = "compio-runtime")]
mod compio_cases {
    use super::*;
    use sockudo_ws::{CompioWebSocketStream, Role};

    fn run(future: impl Future<Output = ()>) {
        compio::runtime::Runtime::new().unwrap().block_on(future);
    }
    async fn pair() -> (compio::net::TcpStream, compio::net::TcpStream) {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = compio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        (stream, peer)
    }
    receive_case!(
        plain_unified_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| CompioWebSocketStream::from_raw_with_leftover(
            io,
            Role::Client,
            config,
            Some(wire)
        ),
        |ws| (ws, ())
    );
    receive_case!(
        plain_split_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| CompioWebSocketStream::from_raw_with_leftover(
            io,
            Role::Client,
            config,
            Some(wire)
        ),
        |ws: CompioWebSocketStream<_>| ws.split()
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_unified_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| sockudo_ws::CompioCompressedWebSocketStream::client_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws| (ws, ())
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_split_honors_text_policy,
        b"\x81\x01\xff",
        |io, config, wire| sockudo_ws::CompioCompressedWebSocketStream::client_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws: sockudo_ws::CompioCompressedWebSocketStream<_>| ws.split()
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_server_unified_honors_text_policy,
        b"\x81\x81\x00\x00\x00\x00\xff",
        |io, config, wire| sockudo_ws::CompioCompressedWebSocketStream::server_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws| (ws, ())
    );

    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        compressed_server_split_honors_text_policy,
        b"\x81\x81\x00\x00\x00\x00\xff",
        |io, config, wire| sockudo_ws::CompioCompressedWebSocketStream::server_with_leftover(
            io,
            config,
            sockudo_ws::DeflateConfig::default(),
            Some(wire)
        ),
        |ws: sockudo_ws::CompioCompressedWebSocketStream<_>| ws.split()
    );
}
