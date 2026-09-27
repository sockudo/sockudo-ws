#![cfg(all(feature = "compio-runtime", feature = "http3"))]

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::runtime::{CancelToken, FutureExt};
use std::task::Poll;
#[path = "support/compio_h3_pair.rs"]
mod pair;

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[compio::test]
async fn zero_capacity_read_does_not_wait(#[case] server: bool) {
    let (mut client, mut peer, endpoint, driver) = pair::pair().await;
    macro_rules! check {
        ($s:expr) => {{
            let mut read = Box::pin($s.read(Vec::<u8>::new()));
            assert!(matches!(
                futures_util::poll!(&mut read),
                Poll::Ready(BufResult(Ok(0), _))
            ));
        }};
    }
    if server {
        check!(peer);
    } else {
        check!(client);
    }
    endpoint.close(0u32.into(), b"done");
    driver.cancel().await;
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[compio::test]
async fn cancelled_read_returns_buffer_then_reads_through_fin(#[case] server: bool) {
    let (client, peer, endpoint, driver) = pair::pair().await;
    let expected: Vec<_> = (0..131073).map(|i| (i % 251) as u8).collect();
    macro_rules! check {
        ($reader:expr, $writer:expr) => {{
            let mut reader = $reader;
            let mut writer = $writer;
            let buffer = Vec::<u8>::with_capacity(7);
            let original = buffer.as_ptr();
            let cancel = CancelToken::new();
            let mut read = Box::pin(reader.read(buffer).with_cancel(cancel.clone()));
            assert!(futures_util::poll!(&mut read).is_pending());
            cancel.cancel();
            let BufResult(result, mut buffer) = read.await;
            assert!(result.is_err());
            assert_eq!(buffer.as_ptr(), original);
            let payload = expected.clone();
            let sending = compio::runtime::spawn(async move {
                let BufResult(result, _) = writer.write_all(payload).await;
                result.unwrap();
                compio::io::AsyncWrite::shutdown(&mut writer).await.unwrap();
                writer
            });
            let mut actual = Vec::new();
            loop {
                let BufResult(result, returned) = reader.read(buffer).await;
                buffer = returned;
                let n = result.unwrap();
                if n == 0 {
                    break;
                }
                actual.extend_from_slice(&buffer[..n]);
                buffer.clear();
            }
            assert_eq!(actual, expected);
            let _writer = sending.await.unwrap();
        }};
    }
    if server {
        check!(peer, client);
    } else {
        check!(client, peer);
    }
    endpoint.close(0u32.into(), b"done");
    driver.cancel().await;
}
