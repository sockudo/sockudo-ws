use super::*;
use crate::DeflateWindowBits;

#[test]
fn role_handles_keep_the_negotiated_pool_shared() {
    let config = DeflateConfig {
        server_max_window_bits: DeflateWindowBits::Bits15,
        client_max_window_bits: DeflateWindowBits::Bits10,
        compression_threshold: 123,
        ..DeflateConfig::default()
    };
    let CompressionContext::Shared { pool: first, .. } =
        CompressionContext::with_config(config.clone(), true, true)
    else {
        panic!("expected shared context");
    };
    let CompressionContext::Shared { pool: second, .. } =
        CompressionContext::with_config(config, true, false)
    else {
        panic!("expected shared context");
    };
    assert!(Arc::ptr_eq(&first.inner, &second.inner));
    assert!(!Arc::ptr_eq(&first.inner.server, &second.inner.client));
    assert!(first.is_server);
    assert!(!second.is_server);
}

#[test]
fn configured_pool_is_released_after_its_split_encoder() {
    let config = DeflateConfig {
        compression_threshold: 321,
        ..DeflateConfig::default()
    };
    let context = CompressionContext::with_config(config, true, true);
    let CompressionContext::Shared { pool, .. } = &context else {
        panic!("expected shared context");
    };
    let weak = Arc::downgrade(&pool.inner);

    let (encoder, _decoder) = context.into_parts();
    assert!(weak.upgrade().is_some());
    drop(encoder);
    assert!(weak.upgrade().is_none());
}

#[test]
fn different_negotiated_configs_do_not_reuse_encoder_settings() {
    let first = DeflateConfig {
        compression_level: 1,
        compression_threshold: 73,
        ..DeflateConfig::default()
    };
    let second = DeflateConfig {
        compression_level: 9,
        compression_threshold: 117,
        client_max_window_bits: DeflateWindowBits::Bits10,
        ..first.clone()
    };
    let contexts = [
        CompressionContext::with_config(first.clone(), true, false),
        CompressionContext::with_config(second.clone(), true, false),
    ];
    let [
        CompressionContext::Shared { pool: a, .. },
        CompressionContext::Shared { pool: b, .. },
    ] = &contexts
    else {
        panic!("expected shared contexts");
    };
    assert!(!Arc::ptr_eq(&a.inner, &b.inner));

    let payload = b"negotiated encoder settings ".repeat(256);
    for (mut context, config) in contexts.into_iter().zip([first, second]) {
        let mut reference = DeflateEncoder::new(
            config.client_max_window_bits,
            true,
            config.compression_level,
            config.compression_threshold,
        );
        assert_eq!(
            context.compress(&payload).unwrap(),
            reference.compress(&payload).unwrap()
        );
        assert!(
            context
                .compress(&vec![b'a'; config.compression_threshold - 1])
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn concurrent_connections_share_one_configured_pool() {
    let ready = Arc::new(std::sync::Barrier::new(4));
    let pools: Vec<_> = (0..4)
        .map(|_| {
            let ready = Arc::clone(&ready);
            std::thread::spawn(move || {
                let config = DeflateConfig {
                    compression_threshold: 567,
                    ..DeflateConfig::default()
                };
                let CompressionContext::Shared { pool, .. } =
                    CompressionContext::with_config(config, true, true)
                else {
                    panic!("expected shared context");
                };
                ready.wait();
                pool
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(
        pools
            .iter()
            .all(|pool| Arc::ptr_eq(&pools[0].inner, &pool.inner))
    );
}
