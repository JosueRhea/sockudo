FROM debian:bullseye-slim
# Or FROM alpine for a smaller image, but may require different dependencies for ca-certificates or other runtime needs (e.g., musl vs glibc)
RUN apt-get update && apt-get upgrade -y && apt-get install -y ca-certificates openssl

# Create a non-root user
RUN groupadd --system sockudo_group && useradd --system --no-log-init -g sockudo_group sockudo_user

WORKDIR /opt/sockudo
ADD https://github.com/RustNSparks/sockudo/releases/download/v1.1.1/sockudo-v1.1.1-x86_64-unknown-linux-gnu.tar.gz /opt/sockudo/
RUN tar -xzf /opt/sockudo/sockudo-v1.1.1-x86_64-unknown-linux-gnu.tar.gz -C . && \
    rm /opt/sockudo/sockudo-v1.1.1-x86_64-unknown-linux-gnu.tar.gz
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
ENTRYPOINT ["./sockudo"]
CMD ["--config=./config.json"]