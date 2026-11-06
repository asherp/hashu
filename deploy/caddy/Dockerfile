# Caddy front for the live demo, with the Slidev deck baked in at /slides.
# Build context is the repo root (see deploy/docker-compose.yml), so a
# `docker compose up -d --build` rebuilds the deck from slides/.

FROM node:22-bookworm-slim AS slides
WORKDIR /slides
COPY slides/package.json slides/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY slides/ ./
RUN npx slidev build --base /slides/ --out /out

FROM caddy:2
COPY --from=slides /out /srv/slides
