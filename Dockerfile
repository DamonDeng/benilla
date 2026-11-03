# Build stage — `rust:1` is the latest stable 1.x (needed: some deps use edition 2024).
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin bridge --locked

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
