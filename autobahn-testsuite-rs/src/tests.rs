use super::*;
use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{CloseFrame, Role, frame::coding::CloseCode},
    },
};

async fn independent_case(id: &str, server: bool, corrupt: bool) -> report::CaseResult {
    let case = catalog::load()
        .unwrap()
        .into_iter()
        .find(|c| c.id == id)
        .unwrap();
    let (local, remote) = tokio::io::duplex(65536);
    let task = tokio::spawn(async move {
        let mut ws = WebSocketStream::from_raw_socket(
            remote,
            if server { Role::Client } else { Role::Server },
            None,
        )
        .await;
        while let Some(message) = ws.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let value = if corrupt {
                        Message::text("wrong")
                    } else {
                        Message::Text(text)
                    };
                    if ws.send(value).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    if ws.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => {
                    let _ = ws.flush().await;
                    break;
                }
                Ok(_) => {
                    let _ = ws.flush().await;
                }
                Err(error) => {
                    let code = if matches!(error, tokio_tungstenite::tungstenite::Error::Utf8(_)) {
                        CloseCode::Invalid
                    } else {
                        CloseCode::Protocol
                    };
                    let _ = ws
                        .close(Some(CloseFrame {
                            code,
                            reason: "invalid".into(),
                        }))
                        .await;
                    break;
                }
            }
        }
    });
    let connection = handshake::Connection {
        io: Box::new(local),
        initial: BytesMut::new(),
        compression: None,
        server,
    };
    let spec = config::Spec {
        message_count: Some(3),
        ..Default::default()
    };
    let result = runner::run(connection, &case, "tungstenite", &spec).await;
    task.await.unwrap();
    result
}
#[tokio::test]
async fn independent_peer_passes_echo_ping_fragmentation_and_large_payload_in_both_roles() {
    for server in [false, true] {
        for id in [
            "1.1.1", "1.1.7", "1.2.8", "2.3", "2.6", "5.15", "6.2.3", "7.7.1", "9.1.1", "9.7.3",
            "10.1.1",
        ] {
            let result = independent_case(id, server, false).await;
            assert!(!result.failed(), "{id} server={server}: {result:#?}");
        }
    }
}
#[tokio::test]
async fn runner_catches_corrupt_echo_and_invalid_input_is_rejected() {
    assert_eq!(
        independent_case("1.1.2", false, true).await.behavior,
        "FAILED"
    );
    for id in ["2.5", "3.1", "4.1.1", "5.1", "6.3.1"] {
        let result = independent_case(id, false, false).await;
        assert!(!result.failed(), "{id}: {result:#?}");
    }
}

#[tokio::test]
async fn reflected_invalid_close_codes_remain_visible_and_informational_cases_stay_informational() {
    for server in [false, true] {
        for (id, code, behavior, behavior_close) in [
            ("7.9.1", 0, "FAILED", "WRONG CODE"),
            ("7.13.1", 5000, "INFORMATIONAL", "INFORMATIONAL"),
            ("7.13.2", 65535, "INFORMATIONAL", "INFORMATIONAL"),
        ] {
            let case = catalog::load()
                .unwrap()
                .into_iter()
                .find(|c| c.id == id)
                .unwrap();
            let (local, peer) = tokio::io::duplex(1024);
            let task = tokio::spawn(async move {
                let (read, write) = tokio::io::split(peer);
                let mut reader = codec::Reader::new(read, BytesMut::new(), !server, 1024);
                let mut writer = codec::Writer::new(write, server);
                while let Some(frame) = reader.next().await.unwrap() {
                    if frame.opcode == 8 {
                        writer.frame(&frame, 0).await.unwrap();
                        writer.flush().await.unwrap();
                        break;
                    }
                }
            });
            let connection = handshake::Connection {
                io: Box::new(local),
                initial: BytesMut::new(),
                compression: None,
                server,
            };
            let result =
                runner::run(connection, &case, "reflect-close", &config::Spec::default()).await;
            task.await.unwrap();
            assert_eq!(
                result.rx_frames, 1,
                "the rejected close frame must be counted"
            );
            assert_eq!(result.rx_bytes, if server { 8 } else { 4 });
            assert_eq!(
                (
                    result.behavior.as_str(),
                    result.behavior_close.as_str(),
                    result.remote_close_code
                ),
                (behavior, behavior_close, Some(code)),
                "{id} server={server}: {result:#?}"
            );
        }
    }
}
#[test]
fn compression_negotiation_is_directional_and_rejects_duplicates() {
    let offers = compression::offer(5);
    let (response, server) = compression::accept(&offers, 5).unwrap().unwrap();
    let client = compression::response(&response, 5).unwrap();
    assert_eq!(server.tx_window, client.rx_window);
    assert_eq!(server.rx_window, client.tx_window);
    assert!(client.rx_no_context);
    assert!(server.rx_no_context);
    assert!(
        compression::response(
            "permessage-deflate; server_max_window_bits=9; server_max_window_bits=15",
            1
        )
        .is_err()
    );
    assert!(compression::response("permessage-deflate; client_max_window_bits", 1).is_err());
}

#[tokio::test]
async fn automatic_fragmentation_preserves_upstream_empty_final_continuation() {
    for (payload, expected) in [
        ("abcdef", vec![(1, false, 3), (0, false, 3), (0, true, 0)]),
        ("abcde", vec![(1, false, 3), (0, true, 2)]),
        ("abc", vec![(1, true, 3)]),
        ("", vec![(1, true, 0)]),
    ] {
        let (local, peer) = tokio::io::duplex(1024);
        let mut writer = codec::Writer::new(local, false);
        writer
            .message(1, Bytes::from_static(payload.as_bytes()), 3, 0)
            .await
            .unwrap();
        writer.flush().await.unwrap();
        drop(writer);
        let mut reader = codec::Reader::new(peer, BytesMut::new(), false, 1024);
        let mut frames = vec![];
        while let Some(frame) = reader.next().await.unwrap() {
            frames.push((frame.opcode, frame.fin, frame.payload.len()));
        }
        assert_eq!(frames, expected, "payload {payload:?}");
    }
}
#[test]
fn utf8_validation_preserves_codepoints_across_fragments_and_fails_early() {
    let mut messages = compression::Messages::new(100, None).unwrap();
    assert!(
        messages
            .push(codec::Frame {
                fin: false,
                rsv: 0,
                opcode: 1,
                payload: Bytes::from_static(&[0xf0, 0x9f])
            })
            .unwrap()
            .is_none()
    );
    assert!(messages.push(codec::Frame::new(9, Bytes::new())).is_err());
    assert_eq!(
        messages
            .push(codec::Frame::new(0, Bytes::from_static(&[0x98, 0x80])))
            .unwrap()
            .unwrap()
            .1
            .as_ref(),
        "😀".as_bytes()
    );
    let mut messages = compression::Messages::new(100, None).unwrap();
    assert!(
        messages
            .push(codec::Frame {
                fin: false,
                rsv: 0,
                opcode: 1,
                payload: Bytes::from_static(&[0xf4, 0x90])
            })
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "full 517-case corpus in both roles; run explicitly in release mode"]
async fn all_registered_cases_run_against_native_testee_in_both_roles() {
    let cases = catalog::load().unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    let mut failures = Vec::new();
    let mut results = Vec::new();
    for server in [false, true] {
        for case in &cases {
            let case = case.clone();
            while tasks.len() >= 8 {
                let result: report::CaseResult = tasks.join_next().await.unwrap().unwrap();
                if result.failed() {
                    failures.push(result.clone());
                }
                results.push(result);
            }
            tasks.spawn(async move {
                let (local, remote) = tokio::io::duplex(65536);
                let parameters = if case.compression() {
                    let (response, params) =
                        compression::accept(&compression::offer(case.parameter), case.parameter)
                            .unwrap()
                            .unwrap();
                    let client = compression::response(&response, case.parameter).unwrap();
                    Some((params, client))
                } else {
                    None
                };
                let (local_params, remote_params) = parameters.map_or((None, None), |(s, c)| {
                    if server {
                        (Some(s), Some(c))
                    } else {
                        (Some(c), Some(s))
                    }
                });
                let spec = config::Spec {
                    message_count: if std::env::var_os("AUTOBAHN_FULL_WORKLOAD").is_some() {
                        None
                    } else {
                        Some(3)
                    },
                    ..Default::default()
                };
                let remote_spec = spec.clone();
                let peer = tokio::spawn(async move {
                    service::echo(
                        handshake::Connection {
                            io: Box::new(remote),
                            initial: BytesMut::new(),
                            compression: remote_params,
                            server: !server,
                        },
                        &remote_spec,
                        None,
                    )
                    .await
                });
                let result = runner::run(
                    handshake::Connection {
                        io: Box::new(local),
                        initial: BytesMut::new(),
                        compression: local_params,
                        server,
                    },
                    &case,
                    if server {
                        "native-client"
                    } else {
                        "native-server"
                    },
                    &spec,
                )
                .await;
                peer.abort();
                result
            });
        }
    }
    while let Some(result) = tasks.join_next().await {
        let result = result.unwrap();
        if result.failed() {
            failures.push(result.clone());
        }
        results.push(result);
    }
    report::write(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("reports/native-smoke"),
        &results,
        &cases,
    )
    .unwrap();
    assert!(
        failures.is_empty(),
        "{} failures: {failures:#?}",
        failures.len()
    );
}

#[tokio::test]
async fn handshake_preserves_pipelined_frames_and_rejects_duplicate_keys() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (local, mut remote) = tokio::io::duplex(65536);
    let task = tokio::spawn(async move {
        remote.write_all(b"GET /runCase?case=1 HTTP/1.1\r\nHost: localhost\r\nUpgrade: WebSocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n\x81\x80\x01\x02\x03\x04").await.unwrap();
        let mut bytes = [0; 1024];
        let n = remote.read(&mut bytes).await.unwrap();
        assert!(
            std::str::from_utf8(&bytes[..n])
                .unwrap()
                .contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
    });
    let request = handshake::request(Box::new(local), 1000).await.unwrap();
    assert_eq!(request.initial.as_ref(), &[0x81, 0x80, 1, 2, 3, 4]);
    request.upgrade(None, &[]).await.unwrap();
    task.await.unwrap();
    let (local, mut remote) = tokio::io::duplex(65536);
    remote
        .write_all(
            b"GET / HTTP/1.1\r\nHost: x\r\nSec-WebSocket-Key: a\r\nSec-WebSocket-Key: b\r\n\r\n",
        )
        .await
        .unwrap();
    assert!(handshake::request(Box::new(local), 1000).await.is_err());
}

#[tokio::test]
async fn invalid_utf8_is_detected_before_advertised_frame_payload_arrives() {
    use tokio::io::AsyncWriteExt;
    for masked in [false, true] {
        let (local, mut remote) = tokio::io::duplex(1024);
        let mut header = [0; 14];
        let key = masked.then_some([3, 5, 7, 11]);
        let n = codec::encode_header(&mut header, false, 0, 1, 1000, key);
        remote.write_all(&header[..n]).await.unwrap();
        let mut invalid = vec![0xf4, 0x90];
        if let Some(key) = key {
            codec::apply_mask(&mut invalid, key, 0);
        }
        remote.write_all(&invalid).await.unwrap();
        let mut reader = codec::Reader::new(local, BytesMut::new(), masked, 65536);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(100), reader.next())
                .await
                .unwrap(),
            Err(Error::Protocol(_))
        ));
    }
}

#[test]
fn default_compression_offer_matches_pinned_autobahn_and_accept_preserves_context() {
    assert_eq!(
        compression::offer(1),
        "permessage-deflate; client_no_context_takeover; client_max_window_bits"
    );
    let (response, p) = compression::accept(&compression::offer(1), 1)
        .unwrap()
        .unwrap();
    assert_eq!(response, "permessage-deflate");
    assert!(!p.rx_no_context);
}

#[tokio::test]
async fn an_unresponsive_peer_cannot_block_the_case_deadline() {
    let mut case = catalog::load()
        .unwrap()
        .into_iter()
        .find(|c| c.id == "9.1.6")
        .unwrap();
    case.timeout_ms = 25;
    let (local, _unresponsive) = tokio::io::duplex(32);
    let result = runner::run(
        handshake::Connection {
            io: Box::new(local),
            initial: BytesMut::new(),
            compression: None,
            server: false,
        },
        &case,
        "unresponsive",
        &config::Spec::default(),
    )
    .await;
    assert_eq!(result.behavior, "FAILED");
    assert!(result.duration < 1000.0);
    assert!(result.result.contains("timed out"));
}
#[test]
fn reports_escape_agent_names_and_cannot_traverse_paths() {
    let dir = std::env::temp_dir().join(format!("autobahn-report-test-{}", rand::random::<u64>()));
    let cases = catalog::load().unwrap();
    let mut result = report::CaseResult::new(
        "../../<script>alert(1)</script>",
        &cases[0],
        &config::Spec::default(),
    );
    result.result = "</pre><script>bad()</script>".into();
    report::write(&dir, &[result], &cases).unwrap();
    let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn common_upstream_specs_load_and_unknown_options_fail_loudly() {
    let dir =
        std::env::temp_dir().join(format!("autobahn-spec-test-{}.json", rand::random::<u64>()));
    std::fs::write(&dir,r#"{"options":{"failByDrop":false},"servers":[{"url":"ws://localhost:9001","options":{"version":18}}],"cases":["*"]}"#).unwrap();
    assert_eq!(config::Spec::load(&dir).unwrap().servers.len(), 1);
    std::fs::write(&dir,r#"{"options":{"connections":10,"batchsize":2,"batchdelay":10,"retrydelay":10},"servers":[{"name":"echo","uri":"ws://localhost:9001","desc":"example"}]}"#).unwrap();
    let spec = config::Spec::load(&dir).unwrap();
    assert_eq!(spec.concurrency, 2);
    assert_eq!(spec.connections, 10);
    assert_eq!(spec.connect_retries, None);
    std::fs::write(&dir, r#"{"options":{"unknown-option":true}}"#).unwrap();
    assert!(config::Spec::load(&dir).is_err());
    std::fs::remove_file(dir).unwrap();
}
