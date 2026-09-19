FROM rust:1.88-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY catalog ./catalog
COPY benches ./benches
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=builder /build/target/release/wstest /usr/local/bin/wstest
COPY LICENSE LICENSE-SERIALIZER NOTICE /usr/share/doc/autobahn-testsuite-rs/
WORKDIR /data
EXPOSE 9001 8080
ENTRYPOINT ["wstest"]
CMD ["-m", "fuzzingserver", "-w", "ws://0.0.0.0:9001"]
