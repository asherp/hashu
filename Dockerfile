# Hashu node image: the `hashu` binary (mint + payment rails + Stratum proxy +
# control API / operator GUI). Used by deploy/docker-compose.yml.
#
#   docker build -t hashu .
#   docker run --rm hashu --help

FROM rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p hashu-cli \
 && cp target/release/hashu /usr/local/bin/hashu

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /home/hashu hashu \
 && mkdir -p /data && chown hashu:hashu /data
COPY --from=build /usr/local/bin/hashu /usr/local/bin/hashu
COPY --chmod=0755 deploy/hashu/entrypoint.sh /usr/local/bin/hashu-entrypoint
USER hashu
# Mint DB + mint seed live here (mint.sqlite, seed.hex). Mount a volume.
ENV HASHU_DATA_DIR=/data RUST_LOG=info
VOLUME ["/data"]
# 8085 mint API, 3333 Stratum, 3334 control API + operator GUI (keep private).
EXPOSE 8085 3333 3334
ENTRYPOINT ["hashu-entrypoint"]
CMD ["mint", "run"]
