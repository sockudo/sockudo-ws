#![cfg(feature = "compio-runtime")]

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, util::Splittable};
use sockudo_ws::{Config, Error};
use std::{cell::Cell, io, rc::Rc};

#[derive(Clone)]
struct RecordingIo(Rc<Cell<usize>>);
impl AsyncRead for RecordingIo {
    async fn read<B: IoBufMut>(&mut self, _buf: B) -> BufResult<usize, B> {
        std::future::pending().await
    }
}
impl AsyncWrite for RecordingIo {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let len = buf.as_init().len();
        self.0.set(self.0.get() + len);
        BufResult(Ok(len), buf)
    }
    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Splittable for RecordingIo {
    type ReadHalf = Self;
    type WriteHalf = Self;
    fn split(self) -> (Self, Self) {
        (self.clone(), self)
    }
}
fn config() -> Config {
    Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .max_backpressure(8)
        .build()
}
macro_rules! limit_case {
    ($name:ident, $make:expr) => {
        #[compio::test]
        async fn $name() {
            let written = Rc::new(Cell::new(0));
            let (mut writer, _guard) = ($make)(RecordingIo(written.clone()));
            assert!(matches!(
                writer.send_text("oversized").await,
                Err(Error::BufferFull)
            ));
            assert_eq!(written.get(), 0);
        }
    };
}
limit_case!(unified_rejects_encoded_overflow, |io| (
    sockudo_ws::CompioWebSocketStream::client(io, config()),
    ()
));
limit_case!(split_rejects_encoded_overflow, |io| {
    let (reader, writer) = sockudo_ws::CompioWebSocketStream::client(io, config()).split();
    (writer, reader)
});
#[cfg(feature = "permessage-deflate")]
limit_case!(compressed_rejects_encoded_overflow, |io| (
    sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default()
    ),
    ()
));
#[cfg(feature = "permessage-deflate")]
limit_case!(compressed_split_rejects_encoded_overflow, |io| {
    let (reader, writer) = sockudo_ws::compio::CompioCompressedWebSocketStream::client(
        io,
        config(),
        sockudo_ws::DeflateConfig::default(),
    )
    .split();
    (writer, reader)
});
