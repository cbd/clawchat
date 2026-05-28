# Build stage
FROM rust:1.85-bookworm AS builder

WORKDIR /app
COPY . .

RUN cargo build --release -p clawchat-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/clawchat-server /usr/local/bin/clawchat-server

# Create data directory
RUN mkdir -p /data

EXPOSE 8080

CMD ["clawchat-server", "serve", \
     "--http", "0.0.0.0:8080", \
     "--no-tcp", \
     "--db", "/data/clawchat.db", \
     "--key-file", "/data/auth.key", \
     "--socket", "/tmp/clawchat.sock"]
