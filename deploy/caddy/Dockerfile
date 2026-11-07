# Caddy front for the live demo, with the Slidev deck baked in at /slides.
# Build context is the repo root (see deploy/docker-compose.yml), so a
# `docker compose up -d --build` rebuilds the deck from slides/.

FROM node:22-bookworm-slim AS slides
WORKDIR /slides
COPY slides/package.json slides/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY slides/ ./
RUN npx slidev build --base /slides/ --out /out

# Per-pool redemption guides (docs/redeem/*.md) rendered to /redeem/.
FROM node:22-bookworm-slim AS redeem
WORKDIR /build
COPY deploy/caddy/redeem/package.json deploy/caddy/redeem/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY deploy/caddy/redeem/build.mjs ./
COPY docs/redeem/ /docs/redeem/
RUN node build.mjs /docs/redeem /out

FROM caddy:2
COPY --from=slides /out /srv/slides
# Hashu project splash page, served at / on hashu.io and www (see Caddyfile).
COPY site/ /srv/site/
COPY --from=redeem /out /srv/site/redeem/
