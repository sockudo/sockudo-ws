//! Controlled production stream APIs; see benches/README.md.
use criterion::{Criterion, criterion_group, criterion_main};
#[path = "support/stream_cases.rs"]
pub mod stream_cases;
use futures_util::StreamExt;
use stream_cases::{Kind, Runtime};
#[path = "support/controlled_io.rs"]
pub mod controlled_io;
#[path = "support/receive_cases.rs"]
pub mod receive_cases;
use receive_cases::receive_case;
fn receive(c: &mut Criterion) {
    sockudo_ws::init_clock();
    receive_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::PlainUnified,
        |io, case: &receive_cases::Case| {
            let ws = sockudo_ws::WebSocketStream::from_raw(io, case.role(), case.config());
            (ws, ())
        }
    );
    receive_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::PlainSplit,
        |io, case: &receive_cases::Case| {
            let ws = sockudo_ws::WebSocketStream::from_raw(io, case.role(), case.config());
            ws.split()
        }
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::CompressedUnified,
        |io, case: &receive_cases::Case| {
            let ws = if case.server {
                sockudo_ws::CompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::CompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            (ws, ())
        }
    );
    #[cfg(feature = "permessage-deflate")]
    receive_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::CompressedSplit,
        |io, case: &receive_cases::Case| {
            let ws = if case.server {
                sockudo_ws::CompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::CompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            ws.split()
        }
    );
}
criterion_group!(benches, receive, send);
criterion_main!(benches);

#[path = "support/write_cases.rs"]
pub mod write_cases;
use futures_util::SinkExt;
use write_cases::write_case;
fn send(c: &mut Criterion) {
    write_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::PlainUnified,
        send,
        |io, case: &write_cases::Case| {
            let ws = sockudo_ws::WebSocketStream::from_raw(io, case.role(), case.config());
            (ws, ())
        }
    );
    write_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::PlainSplit,
        send,
        |io, case: &write_cases::Case| {
            let ws = sockudo_ws::WebSocketStream::from_raw(io, case.role(), case.config());
            let (reader, writer) = ws.split();
            (writer, reader)
        }
    );
    write_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::PlainUnified,
        feed,
        |io, case: &write_cases::Case| {
            let ws = sockudo_ws::WebSocketStream::from_raw(io, case.role(), case.config());
            (ws, ())
        }
    );
    #[cfg(feature = "permessage-deflate")]
    write_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::CompressedUnified,
        send,
        |io, case: &write_cases::Case| {
            let ws = if case.client {
                sockudo_ws::CompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::CompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            (ws, ())
        }
    );
    #[cfg(feature = "permessage-deflate")]
    write_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::CompressedSplit,
        send,
        |io, case: &write_cases::Case| {
            let ws = if case.client {
                sockudo_ws::CompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::CompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            let (reader, writer) = ws.split();
            (writer, reader)
        }
    );
    #[cfg(feature = "permessage-deflate")]
    write_case!(
        c,
        runtime(),
        Runtime::Tokio,
        Kind::CompressedUnified,
        feed,
        |io, case: &write_cases::Case| {
            let ws = if case.client {
                sockudo_ws::CompressedWebSocketStream::client(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            } else {
                sockudo_ws::CompressedWebSocketStream::server(
                    io,
                    case.config(),
                    sockudo_ws::DeflateConfig::default(),
                )
            };
            (ws, ())
        }
    );
}

#[path = "support/corpus.rs"]
pub mod corpus;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
