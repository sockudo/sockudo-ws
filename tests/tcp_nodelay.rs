#![cfg(feature = "tokio-runtime")]

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Http1, Message, client::WebSocketClient, server::WebSocketServer};
use tokio::net::TcpListener;

#[test]
fn tcp_nodelay_is_enabled_by_default() {
    assert!(Config::default().tcp_nodelay);
    assert!(Config::uws_defaults().tcp_nodelay);
    assert!(Config::builder().build().tcp_nodelay);
}

#[test]
fn tcp_nodelay_can_be_disabled_and_reenabled() {
    assert!(!Config::builder().tcp_nodelay(false).build().tcp_nodelay);
    assert!(
        Config::builder()
            .tcp_nodelay(false)
            .tcp_nodelay(true)
            .build()
            .tcp_nodelay
    );
}

#[tokio::test]
async fn http1_tcp_entry_points_support_both_nodelay_settings() {
    for enabled in [false, true] {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let config = Config::builder().tcp_nodelay(enabled).build();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = WebSocketServer::<Http1>::new(config.clone());
            let serving = tokio::spawn(async move {
                server
                    .serve(listener, |mut ws, _| async move {
                        let message = ws.next().await.unwrap().unwrap();
                        ws.send(message).await.unwrap();
                    })
                    .await
            });
            let client = WebSocketClient::<Http1>::new(config);
            let (mut ws, _) = client
                .connect_to_url(&format!("ws://{addr}/"), None)
                .await
                .unwrap();
            ws.send(Message::text("hello")).await.unwrap();
            assert!(matches!(
                ws.next().await.unwrap().unwrap(),
                Message::Text(text) if text == "hello"
            ));
            serving.abort();
            assert!(serving.await.unwrap_err().is_cancelled());
        })
        .await
        .unwrap();
    }
}
