FROM rust:1-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Cache dependency builds separately from the source.
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN useradd -m -u 10001 app
COPY --from=builder /app/target/release/justupload /usr/local/bin/justupload
USER app
ENV PORT=8080 UPLOAD_DIR=/tmp/justupload
EXPOSE 8080
CMD ["justupload"]
