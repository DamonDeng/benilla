# Build from the benilla workspace root (the crate uses the workspace's lockfile and settings):
#   docker build -f crates/warcraft-tunnel/Dockerfile -t warcraft-tunnel .
# `rust:1` is the latest stable 1.x (needed: some deps use edition 2024). The root .dockerignore
# keeps target/, the game data and the reference tables out of the build context.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p warcraft-tunnel --bin bridge --locked

# Runtime stage
FROM debian:bookworm-slim
RUN useradd -r -u 1001 bridge
COPY --from=build /src/target/release/bridge /usr/local/bin/bridge
USER bridge
ENV TUNNEL_LISTEN=0.0.0.0:9090 \
    KEYS_FILE=/etc/warcraft-tunnel/keys.toml \
    REALMD_ADDR=realmd:3724 \
    WORLD_ADDR=mangosd:8085
EXPOSE 9090
ENTRYPOINT ["/usr/local/bin/bridge"]
