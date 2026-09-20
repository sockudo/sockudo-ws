.PHONY: all build build-runner run test clean

# Build the autobahn test server
build:
	cargo build --release --locked --manifest-path ../Cargo.toml --bin autobahn-server --features permessage-deflate

build-runner:
	cargo build --release --locked --manifest-path ../autobahn-testsuite-rs/Cargo.toml --bin wstest

# Run the autobahn test server
run: build
	cd .. && ./target/release/autobahn-server

# Run every case with its full workload using the bundled Rust port.
test: build build-runner
	python3 run.py

# Clean build artifacts
clean:
	cd .. && cargo clean
	rm -rf reports
