FROM rust:latest AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p mangle-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/mangle-server /usr/local/bin/
EXPOSE 8090
ENTRYPOINT ["mangle-server"]
