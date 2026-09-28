//! Allocation diagnostics, deliberately separate from Criterion wall-time binaries.
//! Counts include the operation's construction and retain its returned state.

#[cfg(not(feature = "mimalloc"))]
mod measured {
    #[cfg(feature = "permessage-deflate")]
    use bytes::Bytes;
    use bytes::BytesMut;
    use sockudo_ws::{
        frame::{OpCode, encode_frame},
        protocol::{Protocol, Role},
    };
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        sync::atomic::{AtomicIsize, AtomicUsize, Ordering},
    };

    struct CountingAllocator;
    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
    static LIVE: AtomicIsize = AtomicIsize::new(0);
    static PEAK: AtomicIsize = AtomicIsize::new(0);

    fn allocated(size: usize) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED.fetch_add(size, Ordering::Relaxed);
        let live = LIVE.fetch_add(size as isize, Ordering::Relaxed) + size as isize;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
    // All calls preserve System's layout and pointer contracts. Counters never allocate.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                allocated(layout.size());
            }
            pointer
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                allocated(layout.size());
            }
            pointer
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) };
            LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let result = unsafe { System.realloc(pointer, layout, size) };
            if !result.is_null() {
                LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
                allocated(size);
            }
            result
        }
    }
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    fn measure<T>(name: &str, messages: usize, operation: impl FnOnce() -> T) -> T {
        let allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let bytes = ALLOCATED.load(Ordering::Relaxed);
        let live = LIVE.load(Ordering::Relaxed);
        PEAK.store(live, Ordering::Relaxed);
        let state = operation();
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations;
        let bytes = ALLOCATED.load(Ordering::Relaxed) - bytes;
        let retained = LIVE.load(Ordering::Relaxed) - live;
        let peak = PEAK.load(Ordering::Relaxed) - live;
        println!("{name},{messages},{allocations},{bytes},{peak},{retained}");
        state
    }
    #[cfg(feature = "tokio-runtime")]
    fn receive() {
        use futures_util::StreamExt;
        use std::collections::VecDeque;
        sockudo_ws::init_clock();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut wire = BytesMut::new();
        for size in [32, 65536, 32] {
            encode_frame(&mut wire, OpCode::Binary, &vec![0x42; size], true, None);
        }
        let wire = wire.freeze();
        // Warm shared runtime/fixture state only; each measured connection remains fresh.
        runtime.block_on(async { tokio::task::yield_now().await });
        drop(wire.clone());
        for retain in [0, 16] {
            let state = measure(
                if retain == 0 {
                    "tokio_receive_drop"
                } else {
                    "tokio_receive_hold16"
                },
                96,
                || {
                    runtime.block_on(async {
                        let io = crate::controlled_io::Input::new(wire.clone(), 65536, false);
                        let mut ws = sockudo_ws::WebSocketStream::client(
                            io,
                            sockudo_ws::Config::builder()
                                .auto_ping(false)
                                .idle_timeout(0)
                                .build(),
                        );
                        let mut held = VecDeque::new();
                        for index in 0..96 {
                            let message = ws.next().await.unwrap().unwrap();
                            assert_eq!(message.as_bytes().len(), [32, 65536, 32][index % 3]);
                            held.push_back(message);
                            if held.len() > retain {
                                held.pop_front();
                            }
                        }
                        (ws, held)
                    })
                },
            );
            if retain != 0 {
                let (mut ws, mut held) = state;
                // Release shared payloads before measuring recovery on small messages.
                held.clear();
                let mut small = BytesMut::new();
                encode_frame(&mut small, OpCode::Binary, &[0x42; 32], true, None);
                *ws.get_mut() = crate::controlled_io::Input::new(small.freeze(), 65536, false);
                let state = measure("tokio_receive_released_small", 96, || {
                    runtime.block_on(async {
                        for _ in 0..96 {
                            let message = ws.next().await.unwrap().unwrap();
                            assert_eq!(message.as_bytes(), &[0x42; 32]);
                        }
                        ws
                    })
                });
                drop(state);
            } else {
                drop(state);
            }
        }
    }

    pub fn run() {
        println!(
            "operation,messages,allocation_calls,allocated_bytes,peak_extra_bytes,retained_extra_bytes"
        );
        #[cfg(feature = "tokio-runtime")]
        receive();
        for retain in [false, true] {
            let sizes = [32, 65536, 32];
            let frames: Vec<_> = sizes
                .iter()
                .map(|&size| {
                    let mut frame = BytesMut::new();
                    encode_frame(&mut frame, OpCode::Binary, &vec![0x42; size], true, None);
                    frame.freeze()
                })
                .collect();
            let label = if retain {
                "protocol_hold"
            } else {
                "protocol_drop"
            };
            let state = measure(label, 96, || {
                let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
                let mut buffer = BytesMut::with_capacity(65536);
                let mut held = Vec::new();
                for frame in frames.iter().cycle().take(96) {
                    buffer.extend_from_slice(frame);
                    let messages = protocol.process(&mut buffer).unwrap();
                    assert_eq!(messages.len(), 1);
                    if retain {
                        held.extend(messages);
                    }
                }
                (protocol, buffer, held)
            });
            drop(state);
        }
        #[cfg(feature = "permessage-deflate")]
        for reset in [false, true] {
            use sockudo_ws::deflate::{DeflateDecoder, DeflateEncoder, MAX_WINDOW_BITS};
            let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, reset, 6, 0);
            let payloads: Vec<Bytes> = [256, 65536, 256]
                .into_iter()
                .map(|size| crate::corpus::json_messages(size, 1).pop().unwrap())
                .collect();
            // Codec setup is included below; only shared implementation initialization is warmed.
            let mut warm_encoder = DeflateEncoder::new(MAX_WINDOW_BITS, reset, 6, 0);
            let mut warm_decoder = DeflateDecoder::new(MAX_WINDOW_BITS, reset);
            for payload in &payloads {
                let wire = warm_encoder.compress(payload).unwrap().unwrap();
                assert_eq!(
                    warm_decoder
                        .decompress(&wire, payload.len())
                        .unwrap()
                        .as_ref(),
                    payload.as_ref()
                );
            }
            drop((warm_encoder, warm_decoder));
            let state = measure(
                if reset {
                    "encoder_reset"
                } else {
                    "encoder_takeover"
                },
                96,
                || {
                    let mut encoder = DeflateEncoder::new(MAX_WINDOW_BITS, reset, 6, 0);
                    for payload in payloads.iter().cycle().take(96) {
                        std::hint::black_box(encoder.compress(payload).unwrap().unwrap());
                    }
                    encoder
                },
            );
            drop(state);
            let encoded: Vec<_> = payloads
                .iter()
                .cycle()
                .take(96)
                .map(|payload| encoder.compress(payload).unwrap().unwrap())
                .collect();
            let state = measure(
                if reset {
                    "decoder_reset"
                } else {
                    "decoder_takeover"
                },
                96,
                || {
                    let mut decoder = DeflateDecoder::new(MAX_WINDOW_BITS, reset);
                    for (wire, payload) in encoded.iter().zip(payloads.iter().cycle()) {
                        assert_eq!(
                            decoder.decompress(wire, 1 << 20).unwrap().as_ref(),
                            payload.as_ref()
                        );
                    }
                    decoder
                },
            );
            drop(state);
        }
    }
}

#[cfg(not(feature = "mimalloc"))]
fn main() {
    measured::run();
}

#[cfg(feature = "mimalloc")]
fn main() {
    eprintln!(
        "bench_memory requires a build without mimalloc: the library already installs a global allocator"
    );
    std::process::exit(2);
}

#[cfg(all(feature = "tokio-runtime", not(feature = "mimalloc")))]
#[path = "../benches/support/controlled_io.rs"]
pub mod controlled_io;

#[cfg(all(feature = "permessage-deflate", not(feature = "mimalloc")))]
#[path = "../benches/support/corpus.rs"]
pub mod corpus;
