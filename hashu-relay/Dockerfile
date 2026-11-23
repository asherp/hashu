# Stratum relay image (hashu-relay): holds miners' connections in front of
# the hashu daemon so deploys don't drop them. Used by
# deploy/docker-compose.yml. Built from the workspace root:
#
#   docker build -f hashu-relay/Dockerfile -t hashu-relay .
#
# CI rebuilds it only when hashu-relay/ changes (see images.yml), so keep
# everything it depends on in this directory.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p hashu-relay \
 && cp target/release/hashu-relay /usr/local/bin/hashu-relay

FROM debian:bookworm-slim
RUN useradd --system --no-create-home hashu
COPY --from=build /usr/local/bin/hashu-relay /usr/local/bin/hashu-relay
USER hashu
ENV RUST_LOG=info
EXPOSE 3333
ENTRYPOINT ["hashu-relay"]
