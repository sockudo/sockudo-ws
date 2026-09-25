#![cfg(feature = "tokio-runtime")]

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Http1, Message, client::WebSocketClient, server::WebSocketServer};
use tokio::net::TcpListener;

#[tokio::test]
async fn http1_tcp_entry_points_complete_a_round_trip() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let config = Config::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = WebSocketServer::<Http1>::new(config.clone())
            .protocols(["superchat", "chat"])
            .unwrap();
        let serving = tokio::spawn(async move {
            server
                .serve(listener, |mut ws, handshake| async move {
                    assert_eq!(handshake.protocol.as_deref(), Some("superchat"));
                    let message = ws.next().await.unwrap().unwrap();
                    ws.send(message).await.unwrap();
                })
                .await
        });
        let client = WebSocketClient::<Http1>::new(config);
        let (mut ws, handshake) = client
            .connect_to_url(&format!("ws://{addr}/"), Some("chat, superchat"))
            .await
            .unwrap();
        assert_eq!(handshake.protocol.as_deref(), Some("superchat"));
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
