# Grouper hub container. Builds on any Linux host (the VM on the PC, later a VPS).
#   docker compose -f deploy/compose.yaml up -d --build
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p hub

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data grouper \
    && mkdir -p /data /secrets && chown grouper /data /secrets
COPY --from=build /src/target/release/grouper-hub /usr/local/bin/grouper-hub
USER grouper
ENV GROUPER_DATA=/data \
    GROUPER_BIND=0.0.0.0:8080 \
    RUST_LOG=info
VOLUME /data /secrets
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080"]
ENTRYPOINT ["grouper-hub"]
CMD ["serve"]
