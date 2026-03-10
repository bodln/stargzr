# Build stage
# Switched to musl target for a fully static binary with no glibc dependency,
# and added static file copy so CSS/JS are available at the path baked in by CARGO_MANIFEST_DIR
FROM rust:latest AS builder
RUN rustup target add x86_64-unknown-linux-musl
# musl-tools provides x86_64-linux-musl-gcc, needed to compile C dependencies (like aws-lc-sys) against musl
RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*
WORKDIR /app
# Copy manifests
COPY Cargo.toml Cargo.lock ./
# Copy source code
COPY src ./src
COPY askama.toml ./
# Build the application
RUN cargo build --release --target x86_64-unknown-linux-musl

# Runtime stage
FROM alpine:latest
# Install runtime dependencies
RUN apk add --no-cache \
    # for secure HTTPS
    ca-certificates
WORKDIR /app
# Copy the binary from builder, which leaves behind all the source code, all the Rust compiler tools, all the build artifacts
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/stargzr /app/stargzr
# Copy static files, CARGO_MANIFEST_DIR in the binary resolves to /app (the builder WORKDIR),
# so the binary looks for static assets at /app/src/player/static at runtime
COPY --from=builder /app/src/player/static /app/src/player/static
# Create music directory
RUN mkdir -p /app/music
# Expose the port
EXPOSE 8083
# Set environment variable
ENV MUSIC_PATH=/app/music
# Run the binary
CMD ["/app/stargzr"]