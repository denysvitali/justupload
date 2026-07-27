FROM rust:1-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Cache dependency builds separately from the source.
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/justupload /usr/local/bin/justupload
# Runs as root: Fly mounts the volume at /data owned by root, and the process
# creates its upload directory there on boot.
ENV PORT=8080 UPLOAD_DIR=/data/uploads RUST_LOG=info
EXPOSE 8080
CMD ["justupload"]
