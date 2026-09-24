#![cfg(feature = "http3")]
#[cfg(feature = "http3")]
mod h3_support {
    use std::sync::Once;

    static INSTALL_CRYPTO: Once = Once::new();

    pub fn tls_configs() -> (rustls::ServerConfig, rustls::ClientConfig) {
        INSTALL_CRYPTO.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });

        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

        let mut server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        server_tls.alpn_protocols = vec![b"h3".to_vec()];

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let mut client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"h3".to_vec()];

        (server_tls, client_tls)
    }
}

#[cfg(feature = "compio-runtime")]
#[compio::test]
async fn compio_server_rejects_unsupported_early_data() {
    let (server_tls, _) = h3_support::tls_configs();
    let result = sockudo_ws::compio::CompioHttp3Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        sockudo_ws::Config::builder()
            .http3_enable_0rtt(true)
            .build(),
    )
    .await;
    assert!(matches!(result, Err(sockudo_ws::Error::Http3(message)) if message.contains("0-RTT")));
}

#[cfg(feature = "compio-runtime")]
#[compio::test]
async fn compio_server_rejects_unrepresentable_idle_timeout() {
    let (server_tls, _) = h3_support::tls_configs();
    let result = sockudo_ws::compio::CompioHttp3Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        sockudo_ws::Config::builder()
            .http3_idle_timeout(u64::MAX)
            .build(),
    )
    .await;
    assert!(
        matches!(result, Err(sockudo_ws::Error::Http3(message)) if message.contains("idle timeout"))
    );
}

#[tokio::test]
async fn tokio_server_rejects_unrepresentable_stream_window() {
    let (server_tls, _) = h3_support::tls_configs();
    let result = sockudo_ws::WebSocketServer::<sockudo_ws::Http3>::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        sockudo_ws::Config::builder()
            .http3_stream_window_size(u64::MAX)
            .build(),
    )
    .await;
    assert!(
        matches!(result, Err(sockudo_ws::Error::Http3(message)) if message.contains("stream window"))
    );
}

#[tokio::test]
async fn tokio_server_rejects_invalid_udp_payload_size() {
    let (server_tls, _) = h3_support::tls_configs();
    let result = sockudo_ws::WebSocketServer::<sockudo_ws::Http3>::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        sockudo_ws::Config::builder()
            .http3_max_udp_payload_size(1199)
            .build(),
    )
    .await;
    assert!(
        matches!(result, Err(sockudo_ws::Error::Http3(message)) if message.contains("UDP payload"))
    );
}

#[tokio::test]
async fn tokio_client_rejects_disabled_extended_connect_before_connecting() {
    let (_, client_tls) = h3_support::tls_configs();
    let client = sockudo_ws::WebSocketClient::<sockudo_ws::Http3>::new(
        sockudo_ws::Config::builder()
            .http3_enable_connect_protocol(false)
            .build(),
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        client.connect("127.0.0.1:9".parse().unwrap(), "localhost", "/", client_tls),
    )
    .await
    .expect("configuration must be rejected before network I/O");
    assert!(matches!(
        result,
        Err(sockudo_ws::Error::ExtendedConnectNotSupported)
    ));
}

#[tokio::test]
async fn disabled_server_extended_connect_rejects_a_real_request() {
    use sockudo_ws::{Config, Error, Http3, WebSocketClient, WebSocketServer};
    use std::sync::Arc;

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (server_tls, client_tls) = h3_support::tls_configs();
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap();
        let endpoint = quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(crypto)),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let addr = endpoint.local_addr().unwrap();
        let server = WebSocketServer::<Http3>::from_endpoint(
            endpoint.clone(),
            Config::builder()
                .http3_enable_connect_protocol(false)
                .build(),
        );
        let serving = tokio::spawn(async move {
            server
                .serve(|_, _| async { panic!("disabled CONNECT reached handler") })
                .await
        });
        let client = WebSocketClient::<Http3>::new(Config::default());

        let result = client.connect(addr, "localhost", "/", client_tls).await;

        assert!(matches!(result, Err(Error::ExtendedConnectNotSupported)));
        endpoint.close(quinn::VarInt::from_u32(0x100), b"done");
        serving.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[cfg(feature = "compio-runtime")]
#[compio::test]
async fn compio_multiplexed_client_rejects_early_data_before_connecting() {
    let (_, client_tls) = h3_support::tls_configs();
    let result = compio::time::timeout(
        std::time::Duration::from_secs(1),
        sockudo_ws::compio::connect_http3_multiplexed(
            "127.0.0.1:9".parse().unwrap(),
            "localhost",
            client_tls,
            sockudo_ws::Config::builder()
                .http3_enable_0rtt(true)
                .build(),
        ),
    )
    .await
    .expect("configuration must be rejected before network I/O");
    assert!(matches!(result, Err(sockudo_ws::Error::Http3(message)) if message.contains("0-RTT")));
}

#[cfg(feature = "compio-runtime")]
#[compio::test]
async fn compio_multiplexed_client_rejects_disabled_connect_before_connecting() {
    let (_, client_tls) = h3_support::tls_configs();
    let result = compio::time::timeout(
        std::time::Duration::from_secs(1),
        sockudo_ws::compio::connect_http3_multiplexed(
            "127.0.0.1:9".parse().unwrap(),
            "localhost",
            client_tls,
            sockudo_ws::Config::builder()
                .http3_enable_connect_protocol(false)
                .build(),
        ),
    )
    .await
    .expect("configuration must be rejected before network I/O");
    assert!(matches!(
        result,
        Err(sockudo_ws::Error::ExtendedConnectNotSupported)
    ));
}

#[rstest::rstest]
#[case::server(true, false)]
#[case::client(false, false)]
#[case::multiplexed_client(false, true)]
#[tokio::test]
async fn tokio_configured_idle_timeout_closes_an_established_connection(
    #[case] configure_server: bool,
    #[case] multiplexed: bool,
) {
    use futures_util::StreamExt;
    use sockudo_ws::{Config, Http3, WebSocketClient, WebSocketServer};

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (server_tls, client_tls) = h3_support::tls_configs();
        let server_config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .http3_idle_timeout(if configure_server { 100 } else { 30_000 })
            .build();
        let server = WebSocketServer::<Http3>::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_tls,
            server_config,
        )
        .await
        .unwrap();
        let addr = server.local_addr().unwrap();
        let serving = tokio::spawn(server.serve(|mut ws, _| async move {
            // Keep the established stream alive without producing traffic.
            let _ = ws.next().await;
        }));
        let client = WebSocketClient::<Http3>::new(
            Config::builder()
                .auto_ping(false)
                .idle_timeout(0)
                .http3_idle_timeout(if configure_server { 30_000 } else { 100 })
                .build(),
        );
        let mut mux = if multiplexed {
            Some(
                client
                    .connect_multiplexed(addr, "localhost", client_tls.clone())
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let mut ws = if let Some(mux) = &mut mux {
            mux.open_websocket("/idle", None).await.unwrap()
        } else {
            client
                .connect(addr, "localhost", "/idle", client_tls)
                .await
                .unwrap()
        };

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await;

        serving.abort();
        assert!(
            matches!(result, Ok(None) | Ok(Some(Err(_)))),
            "configured QUIC idle timeout did not close the stream: {result:?}"
        );
    })
    .await
    .unwrap();
}

#[cfg(feature = "compio-runtime")]
#[rstest::rstest]
#[case::server(true, false)]
#[case::client(false, false)]
#[case::multiplexed_client(false, true)]
#[compio::test]
async fn compio_configured_idle_timeout_closes_an_established_connection(
    #[case] configure_server: bool,
    #[case] multiplexed: bool,
) {
    use sockudo_ws::Config;
    use sockudo_ws::compio::{CompioHttp3Server, connect_http3, connect_http3_multiplexed};

    compio::time::timeout(std::time::Duration::from_secs(5), async {
        let (server_tls, client_tls) = h3_support::tls_configs();
        let server_config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .http3_idle_timeout(if configure_server { 100 } else { 30_000 })
            .build();
        let server =
            CompioHttp3Server::bind("127.0.0.1:0".parse().unwrap(), server_tls, server_config)
                .await
                .unwrap();
        let addr = server.local_addr().unwrap();
        let serving = compio::runtime::spawn(async move {
            server
                .serve(|mut ws, _| async move {
                    // Keep the established stream alive without producing traffic.
                    let _ = ws.next().await;
                })
                .await
                .unwrap()
        });
        let config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .http3_idle_timeout(if configure_server { 30_000 } else { 100 })
            .build();
        let mut mux = if multiplexed {
            Some(
                connect_http3_multiplexed(addr, "localhost", client_tls.clone(), config.clone())
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let mut ws = if let Some(mux) = &mut mux {
            mux.open_websocket("/idle", None).await.unwrap()
        } else {
            connect_http3(addr, "localhost", "/idle", None, client_tls, config)
                .await
                .unwrap()
        };

        let result = compio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await;

        serving.cancel().await;
        assert!(
            matches!(result, Ok(None) | Ok(Some(Err(_)))),
            "configured QUIC idle timeout did not close the stream: {result:?}"
        );
    })
    .await
    .unwrap();
}
