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
