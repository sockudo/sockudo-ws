use autobahn_testsuite::{
    codec::{apply_mask, decode_frame, encode_header},
    compression::{Deflate, Parameters},
};
use bytes::BytesMut;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};
fn measure(mut work: impl FnMut()) -> (usize, f64) {
    let start = Instant::now();
    let mut count = 0;
    loop {
        for _ in 0..1024 {
            work();
        }
        count += 1024;
        if start.elapsed() >= Duration::from_millis(250) {
            break;
        }
    }
    (count, start.elapsed().as_secs_f64())
}
fn row(operation: &str, bytes: usize, count: usize, seconds: f64) {
    println!(
        "{operation},{bytes},{count},{seconds:.6},{:.3}",
        bytes as f64 * count as f64 / seconds / (1u64 << 30) as f64
    );
}
fn main() {
    println!("operation,bytes,iterations,seconds,gib_per_second");
    for size in [125, 4096, 65536, 1048576] {
        let mut data = vec![42; size];
        let (count, seconds) =
            measure(|| apply_mask(black_box(&mut data), black_box([1, 2, 3, 4]), 0));
        row("mask", size, count, seconds);
        let mut header = [0; 14];
        let n = encode_header(&mut header, true, 0, 2, size as u64, None);
        let mut wire = BytesMut::from(&header[..n]);
        wire.extend_from_slice(&data);
        let (count, seconds) = measure(|| {
            let mut copy = wire.clone();
            black_box(decode_frame(&mut copy, false, 2 << 20).unwrap());
        });
        row("decode_with_input_copy", size, count, seconds);
    }
    let mut compressor = Deflate::new(Parameters::default()).unwrap();
    let input = b"Autobahn compression roundtrip payload. ".repeat(1024);
    let (count, seconds) = measure(|| {
        black_box(compressor.encode(black_box(&input)).unwrap());
    });
    row(
        "deflate_repetitive_context_takeover",
        input.len(),
        count,
        seconds,
    );
}
