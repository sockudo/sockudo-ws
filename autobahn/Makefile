.PHONY: all build run test clean

# Build the autobahn test server
build:
	cd .. && cargo build --release --bin autobahn-server --features permessage-deflate

# Run the autobahn test server
run: build
	cd .. && ./target/release/autobahn-server

# Run the Autobahn test suite (requires Docker)
test: build
	@echo "Starting autobahn server..."
	@cd .. && ./target/release/autobahn-server &
	@sleep 2
	@echo "Running Autobahn test suite..."
	docker run -it --rm \
		--network host \
		-v $(PWD):/config \
		-v $(PWD)/reports:/reports \
		crossbario/autobahn-testsuite \
		wstest -m fuzzingclient -s /config/fuzzingclient.json
	@echo "Stopping server..."
	@pkill -f autobahn-server || true
	@echo "Results available in reports/index.html"

# Clean build artifacts
clean:
	cd .. && cargo clean
	rm -rf reports
