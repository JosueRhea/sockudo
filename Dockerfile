# Stage 1: Build the application
FROM rust:1.85 AS builder
# Or a more recent stable Rust version like rust:1.78
WORKDIR /usr/src/sockudo
COPY . .
# Consider using cargo-chef for optimized Docker layer caching if build times are an issue
RUN cargo build --release

# Stage 2: Create the runtime image
FROM debian:bullseye-slim
# Or FROM alpine for a smaller image, but may require different dependencies for ca-certificates or other runtime needs (e.g., musl vs glibc)
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

# Create a non-root user
RUN groupadd --system sockudo_group && useradd --system --no-log-init -g sockudo_group sockudo_user

WORKDIR /opt/sockudo
COPY --from=builder /usr/src/sockudo/target/release/sockudo .
# Copy your default config.json if you want it baked into the image,
# but it's generally better to mount it or use environment variables.
# COPY config.json.production ./config.json

# Ensure the binary is executable
RUN chmod +x ./sockudo

# Change ownership to the non-root user
# This step might be redundant if WORKDIR is created after USER, but good for clarity
RUN chown -R sockudo_user:sockudo_group /opt/sockudo

USER sockudo_user

# Expose the default Sockudo port and metrics port
EXPOSE 6001
EXPOSE 9601

# Default command (can be overridden)
# Ensure config.json is present at this path in the container (e.g., mounted as a volume) or configure via ENV vars
CMD ["./sockudo", "--config=./config.json"]