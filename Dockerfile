# Build stage
FROM rust:1.89 AS builder

WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src
COPY askama.toml ./

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && \
    # for secure HTTPS
    apt-get install -y ca-certificates && \ 
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the binary from builder, which leaves behind all the source code, all the Rust compiler tools, all the build artifacts
COPY --from=builder /app/target/release/stargzr /app/stargzr

# Create music directory
RUN mkdir -p /app/music

# Expose the port
EXPOSE 8083

# Set environment variable
ENV MUSIC_PATH=/app/music

# Run the binary
CMD ["/app/stargzr"]