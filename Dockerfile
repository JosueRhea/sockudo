FROM rust:1.85

WORKDIR /usr/src/sockudo

# Install runtime dependencies
RUN apt-get update && apt-get upgrade -y && \
    apt-get install -y ca-certificates openssl && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Create a non-root user
RUN groupadd --system sockudo_group && useradd --system --no-log-init -g sockudo_group sockudo_user

COPY . .

# Build the application
RUN cargo build --release

WORKDIR /opt/sockudo

# Move the binary to the final location
RUN mv /usr/src/sockudo/target/release/sockudo .

# Ensure the binary is executable
RUN chmod +x ./sockudo

# Change ownership to the non-root user
RUN chown -R sockudo_user:sockudo_group /opt/sockudo

USER sockudo_user

# Expose the default Sockudo port and metrics port
EXPOSE 6001
EXPOSE 9601

# Default command (can be overridden)
CMD ["./sockudo", "--config=./config.json"]