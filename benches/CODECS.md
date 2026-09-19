# Codec and fan-out benchmarks

All payloads are generated deterministically; no external corpus is required.
Run a selected suite with `cargo bench --bench NAME` and use identical profiles,
features and independent target directories for baseline/candidate comparisons.

- `protocol_bench`: framing, fragmented messages and protocol processing.
- `masking_bench` and `client_mask_bench`: in-place and copy masking by size and alignment.
- `deflate_input_bench`: reset/takeover, mixed/repeated input and TCP delivery.
- `deflate_capacity_bench`: fixed/alternating sizes, capacity growth and TCP batches.
- `services_bench`: compression and publication with receiver draining included.
- Existing `websocket_bench` adds size/alignment boundaries; `comparison_bench`
  validates the roundtrip with separate sender/receiver protocol state.

For smoke validation, Criterion supports `--test`; smoke results are not timings.
For async multi-worker TCP trials, provide separate physical cores for workers,
producer and peer rather than pinning the whole process to a single CPU.
Report latency and allocation/capacity changes separately from throughput.
