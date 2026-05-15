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

#[cfg(all(feature = "tokio-runtime", feature = "http2"))]
mod tokio_http2_e2e {
    use futures_util::{SinkExt, StreamExt};
    use sockudo_ws::{Config, Http2, Message, WebSocketClient, WebSocketServer};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn websocket_over_http2_tcp_e2e() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            WebSocketServer::<Http2>::new(Config::default())
                .serve(stream, |mut ws, req| async move {
                    assert_eq!(req.path, "/tokio-h2");
                    let msg = ws.next().await.unwrap().unwrap();
                    assert!(matches!(&msg, Message::Text(text) if text == "tokio-h2"));
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let client = WebSocketClient::<Http2>::new(Config::default());
        let mut ws = client
            .connect(
                stream,
                &format!("https://localhost:{}/tokio-h2", addr.port()),
                Some("sockudo.e2e"),
            )
            .await
            .unwrap();

        ws.send(Message::text("tokio-h2")).await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "tokio-h2"));

        drop(ws);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn multiplexed_websockets_over_http2_tcp_e2e() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            WebSocketServer::<Http2>::new(Config::default())
                .serve(stream, |mut ws, req| async move {
                    assert!(matches!(req.path.as_str(), "/one" | "/two"));
                    let msg = ws.next().await.unwrap().unwrap();
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let client = WebSocketClient::<Http2>::new(Config::default());
        let mut mux = client.connect_multiplexed(stream).await.unwrap();

        let mut one = mux
            .open_websocket(&format!("https://localhost:{}/one", addr.port()), None)
            .await
            .unwrap();
        let mut two = mux
            .open_websocket(&format!("https://localhost:{}/two", addr.port()), None)
            .await
            .unwrap();

        one.send(Message::text("one")).await.unwrap();
        two.send(Message::text("two")).await.unwrap();

        let echoed = one.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "one"));
        let echoed = two.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "two"));

        drop(one);
        drop(two);
        drop(mux);
        server_task.await.unwrap();
    }
}

#[cfg(all(feature = "tokio-runtime", feature = "http3"))]
mod tokio_http3_e2e {
    use std::sync::Arc;

    use futures_util::{SinkExt, StreamExt};
    use sockudo_ws::{Config, Http3, Message, WebSocketClient, WebSocketServer};

    fn server_endpoint(server_tls: rustls::ServerConfig) -> quinn::Endpoint {
        let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap();
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_config));
        quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn websocket_over_http3_quic_e2e() {
        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls);
        let addr = endpoint.local_addr().unwrap();
        let server = WebSocketServer::<Http3>::from_endpoint(endpoint.clone(), Config::default());

        let server_task = tokio::spawn(async move {
            server
                .serve(|mut ws, req| async move {
                    assert_eq!(req.path, "/tokio-h3");
                    let msg = ws.next().await.unwrap().unwrap();
                    assert!(matches!(&msg, Message::Text(text) if text == "tokio-h3"));
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let client = WebSocketClient::<Http3>::new(Config::default());
        let mut ws = client
            .connect(addr, "localhost", "/tokio-h3", client_tls)
            .await
            .unwrap();

        ws.send(Message::text("tokio-h3")).await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "tokio-h3"));

        drop(ws);
        endpoint.close(quinn::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn multiplexed_websockets_over_http3_quic_e2e() {
        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls);
        let addr = endpoint.local_addr().unwrap();
        let server = WebSocketServer::<Http3>::from_endpoint(endpoint.clone(), Config::default());

        let server_task = tokio::spawn(async move {
            server
                .serve(|mut ws, req| async move {
                    assert!(matches!(req.path.as_str(), "/one" | "/two"));
                    let msg = ws.next().await.unwrap().unwrap();
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let client = WebSocketClient::<Http3>::new(Config::default());
        let mut mux = client
            .connect_multiplexed(addr, "localhost", client_tls)
            .await
            .unwrap();

        let mut one = mux.open_websocket("/one", None).await.unwrap();
        let mut two = mux.open_websocket("/two", None).await.unwrap();

        one.send(Message::text("one")).await.unwrap();
        two.send(Message::text("two")).await.unwrap();

        let echoed = one.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "one"));
        let echoed = two.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "two"));

        drop(one);
        drop(two);
        drop(mux);
        endpoint.close(quinn::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }
}

#[cfg(feature = "compio-runtime")]
mod compio_http1_e2e {
    use sockudo_ws::compio::net::{TcpListener, TcpStream};
    use sockudo_ws::compio::{accept_async, connect_async, runtime};
    use sockudo_ws::{Config, Message};

    #[compio::test]
    async fn websocket_over_http1_tcp_e2e() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut ws, req) = accept_async(stream, Config::default()).await.unwrap();
            assert_eq!(req.path, "/compio-h1");
            let msg = ws.next().await.unwrap().unwrap();
            assert!(matches!(&msg, Message::Text(text) if text == "compio-h1"));
            ws.send(msg).await.unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut ws, req) = connect_async(
            stream,
            &addr.to_string(),
            "/compio-h1",
            Some("sockudo.e2e"),
            Config::default(),
        )
        .await
        .unwrap();
        assert_eq!(req.path, "/compio-h1");

        ws.send_text("compio-h1").await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "compio-h1"));

        server_task.await.unwrap();
    }
}

#[cfg(all(feature = "compio-runtime", feature = "http2"))]
mod compio_http2_e2e {
    use sockudo_ws::compio::net::{TcpListener, TcpStream};
    use sockudo_ws::compio::{connect_http2, connect_http2_multiplexed, runtime, serve_http2};
    use sockudo_ws::{Config, Message};

    #[compio::test]
    async fn websocket_over_http2_tcp_e2e() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_http2(stream, Config::default(), |mut ws, req| async move {
                assert_eq!(req.path, "/compio-h2");
                let msg = ws.next().await.unwrap().unwrap();
                assert!(matches!(&msg, Message::Text(text) if text == "compio-h2"));
                ws.send(msg).await.unwrap();
            })
            .await
            .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut ws = connect_http2(
            stream,
            &format!("https://localhost:{}/compio-h2", addr.port()),
            Some("sockudo.e2e"),
            Config::default(),
        )
        .await
        .unwrap();

        ws.send_text("compio-h2").await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "compio-h2"));

        drop(ws);
        server_task.await.unwrap();
    }

    #[compio::test]
    async fn multiplexed_websockets_over_http2_tcp_e2e() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_http2(stream, Config::default(), |mut ws, req| async move {
                assert!(matches!(req.path.as_str(), "/one" | "/two"));
                let msg = ws.next().await.unwrap().unwrap();
                ws.send(msg).await.unwrap();
            })
            .await
            .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut mux = connect_http2_multiplexed(stream, Config::default())
            .await
            .unwrap();

        let mut one = mux
            .open_websocket(&format!("https://localhost:{}/one", addr.port()), None)
            .await
            .unwrap();
        let mut two = mux
            .open_websocket(&format!("https://localhost:{}/two", addr.port()), None)
            .await
            .unwrap();

        one.send_text("one").await.unwrap();
        two.send_text("two").await.unwrap();

        let echoed = one.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "one"));
        let echoed = two.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "two"));

        drop(one);
        drop(two);
        drop(mux);
        server_task.await.unwrap();
    }
}

#[cfg(all(feature = "compio-runtime", feature = "http3"))]
mod compio_http3_e2e {
    use sockudo_ws::compio::{
        CompioHttp3Server, connect_http3, connect_http3_multiplexed, runtime,
    };
    use sockudo_ws::{Config, Message};

    async fn server_endpoint(server_tls: rustls::ServerConfig) -> compio::quic::Endpoint {
        compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
            .with_alpn_protocols(&["h3"])
            .bind("127.0.0.1:0")
            .await
            .unwrap()
    }

    #[compio::test]
    async fn websocket_over_http3_quic_e2e() {
        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls).await;
        let addr = endpoint.local_addr().unwrap();
        let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default());

        let server_task = runtime::spawn(async move {
            server
                .serve(|mut ws, req| async move {
                    assert_eq!(req.path, "/compio-h3");
                    let msg = ws.next().await.unwrap().unwrap();
                    assert!(matches!(&msg, Message::Text(text) if text == "compio-h3"));
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let mut ws = connect_http3(
            addr,
            "localhost",
            "/compio-h3",
            Some("sockudo.e2e"),
            client_tls,
            Config::default(),
        )
        .await
        .unwrap();

        ws.send_text("compio-h3").await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "compio-h3"));

        drop(ws);
        endpoint.close(compio::quic::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }

    #[compio::test]
    async fn multiplexed_websockets_over_http3_quic_e2e() {
        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls).await;
        let addr = endpoint.local_addr().unwrap();
        let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default());

        let server_task = runtime::spawn(async move {
            server
                .serve(|mut ws, req| async move {
                    assert!(matches!(req.path.as_str(), "/one" | "/two"));
                    let msg = ws.next().await.unwrap().unwrap();
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let mut mux = connect_http3_multiplexed(addr, "localhost", client_tls, Config::default())
            .await
            .unwrap();

        let mut one = mux.open_websocket("/one", None).await.unwrap();
        let mut two = mux.open_websocket("/two", None).await.unwrap();

        one.send_text("one").await.unwrap();
        two.send_text("two").await.unwrap();

        let echoed = one.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "one"));
        let echoed = two.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "two"));

        drop(one);
        drop(two);
        mux.close();
        endpoint.close(compio::quic::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }
}
