#![cfg(all(feature = "compio-runtime", feature = "http2"))]

use compio::io::{AsyncReadExt, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use rstest::rstest;
use sockudo_ws::compio::{connect_http2_multiplexed, runtime, serve_http2};
use sockudo_ws::{Config, Error};

#[rstest]
#[compio::test]
async fn compio_endpoints_advertise_configured_frame_size(#[values(false, true)] server: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = runtime::spawn(async move {
        let stream = TcpStream::connect(address).await.unwrap();
        let config = Config::builder().http2_max_frame_size(262_144).build();
        if server {
            let _ = serve_http2(stream, config, |_, _| async {}).await;
        } else {
            let connection = connect_http2_multiplexed(stream, config).await.unwrap();
            std::future::pending::<()>().await;
            drop(connection);
        }
    });
    let (mut peer, _) = listener.accept().await.unwrap();
    if server {
        peer.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec())
            .await
            .0
            .unwrap();
    } else {
        let result = peer.read_exact(vec![0; 24]).await;
        result.0.unwrap();
        assert_eq!(&result.1, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    }
    let result = peer.read_exact(vec![0; 9]).await;
    result.0.unwrap();
    let header = result.1;
    assert_eq!(header[3], 4);
    let len = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
    let result = peer.read_exact(vec![0; len]).await;
    result.0.unwrap();
    let setting = result
        .1
        .as_chunks::<6>()
        .0
        .iter()
        .find(|entry| entry[..2] == [0, 5])
        .unwrap();
    assert_eq!(
        u32::from_be_bytes(setting[2..].try_into().unwrap()),
        262_144
    );
    drop(task);
}

#[rstest]
#[compio::test]
async fn compio_rejects_invalid_frame_size(#[values(false, true)] server: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let config = Config::builder().http2_max_frame_size(16_383).build();
    let result = if server {
        serve_http2(stream, config, |_, _| async {}).await
    } else {
        connect_http2_multiplexed(stream, config).await.map(drop)
    };
    assert!(matches!(result, Err(Error::HandshakeFailed(_))));
}
