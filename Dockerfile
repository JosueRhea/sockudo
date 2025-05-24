FROM ubuntu:24.04
# Or FROM alpine for a smaller image, but may require different dependencies for ca-certificates or other runtime needs (e.g., musl vs glibc)
RUN apt-get update && apt-get upgrade -y && apt-get install -y ca-certificates openssl

# Create a non-root user
# RUN groupadd --system sockudo_group && useradd --system --no-log-init -g sockudo_group sockudo_user

WORKDIR /opt/sockudo
ADD https://github.com/RustNSparks/sockudo/releases/download/v1.1.1/sockudo-v1.1.1-x86_64-unknown-linux-gnu.tar.gz /opt/sockudo/
RUN tar -xzf /opt/sockudo/sockudo-v1.1.1-x86_64-unknown-linux-gnu.tar.gz -C . && \
    rm /opt/sockudo/sockudo-v1.1.1-x86_64-unknown-linux-gnu.tar.gz
# Copy your default config.json if you want it baked into the image,
# but it's generally better to mount it or use environment variables.
# COPY config.json.production ./config.json

# Ensure the binary is executable
RUN chmod +x ./sockudo

COPY config.json /opt/sockudo/config.json

# Change ownership to the non-root user
# This step might be redundant if WORKDIR is created after USER, but good for clarity
# RUN chown -R sockudo_user:sockudo_group /opt/sockudo

# USER sockudo_user

# Expose the default Sockudo port and metrics port
EXPOSE 6001
EXPOSE 9601

# Default command (can be overridden)
# Ensure config.json is present at this path in the container (e.g., mounted as a volume) or configure via ENV vars
# ENTRYPOINT ["./sockudo"]
CMD ["./sockudo","--config=./config.json"]


# {
#   "debug": false,
#   "port": 6001,
#   "host": "127.0.0.1",
#   "cors": {
#     "credentials": false,
#     "origin": ["*"],
#     "methods": ["GET", "POST", "OPTIONS"],
#     "allowed_headers": [
#       "Authorization",
#       "Content-Type",
#       "X-Requested-With",
#       "Accept"
#     ]
#   },
#   "app_manager": {
#     "driver": "memory",
#     "array": {
#       "apps": [
#         {
#           "id": "app1",
#           "key": "1234567890",
#           "secret": "1234567890",
#           "enable_client_messages": true,
#           "enabled": true,
#           "max_connections": "1000",
#           "max_client_events_per_second": "10"
#         }
#       ]
#     }
#   },
#   "adapter": {
#     "driver": "local",
#     "nats": {
#       "requests_timeout": 5000,
#       "prefix": "sockudo",
#       "servers": ["nats://nats-production-8e2d.up.railway.app"],
#       "connection_timeout_ms": 5000
#     }
#   }
# }