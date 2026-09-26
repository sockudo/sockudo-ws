#![cfg(all(feature = "tokio-runtime", feature = "http2"))]

use rstest::rstest;
use sockudo_ws::{Config, Error, Http2, WebSocketClient, WebSocketServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn start_endpoint(
    io: tokio::io::DuplexStream,
    endpoint: u8,
    config: Config,
) -> sockudo_ws::Result<()> {
    match endpoint {
        0 => WebSocketClient::<Http2>::new(config)
            .connect(io, "https://localhost/", None)
            .await
            .map(drop),
        1 => WebSocketClient::<Http2>::new(config)
            .connect_multiplexed(io)
            .await
            .map(drop),
        2 => {
            WebSocketServer::<Http2>::new(config)
                .serve(io, |_, _| async {})
                .await
        }
        3 => {
            WebSocketServer::<Http2>::new(config)
                .serve_with_filter(io, |_| true, |_, _| async {})
                .await
        }
        _ => unreachable!(),
    }
}

#[rstest]
#[tokio::test]
async fn configured_frame_size_is_sent_in_settings(
    #[values(0, 1, 2, 3)] endpoint: u8,
    #[values(16_384, 65_536, 262_144)] frame_size: u32,
) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let config = Config::builder().http2_max_frame_size(frame_size).build();
        let (local, mut peer) = tokio::io::duplex(4096);
        let task = tokio::spawn(start_endpoint(local, endpoint, config));
        if endpoint < 2 {
            let mut preface = [0; 24];
            peer.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
        } else {
            peer.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                .await
                .unwrap();
        }
        let mut header = [0; 9];
        peer.read_exact(&mut header).await.unwrap();
        assert_eq!(header[3], 4, "first frame must be SETTINGS");
        let len = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
        let mut settings = vec![0; len];
        peer.read_exact(&mut settings).await.unwrap();
        let advertised = settings
            .as_chunks::<6>()
            .0
            .iter()
            .find(|entry| entry[..2] == [0, 5])
            .unwrap();
        assert_eq!(
            u32::from_be_bytes(advertised[2..].try_into().unwrap()),
            frame_size
        );
        task.abort();
    })
    .await
    .unwrap();
}

#[rstest]
#[tokio::test]
async fn invalid_frame_sizes_return_errors_before_io(
    #[values(0, 1, 2, 3)] endpoint: u8,
    #[values(0, 16_383, 16_777_216)] frame_size: u32,
) {
    let config = Config::builder().http2_max_frame_size(frame_size).build();
    let (local, _peer) = tokio::io::duplex(64);
    assert!(matches!(
        start_endpoint(local, endpoint, config).await,
        Err(Error::HandshakeFailed(_))
    ));
}
