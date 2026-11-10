# Caddy front for the live demo: deploy/Caddyfile, with the Slidev deck baked
# in at /slides.
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
# The routes ship with the image, so the Caddyfile always matches the site,
# slides and guides built here: `docker compose pull && up -d` alone picks
# up a route change. To try a local edit without rebuilding, mount it over
# this path (see the comment on the caddy service in docker-compose.yml).
COPY deploy/Caddyfile /etc/caddy/Caddyfile
# Fails fast on a missing HASHU_DOMAIN / OPERATOR_PASSWORD_HASH and writes
# the optional MDK routes (HASHU_MDK_DOMAIN) that the Caddyfile imports.
COPY --chmod=0755 deploy/caddy/entrypoint.sh /usr/local/bin/hashu-caddy-entrypoint
ENTRYPOINT ["hashu-caddy-entrypoint"]
CMD ["caddy", "run", "--config", "/etc/caddy/Caddyfile", "--adapter", "caddyfile"]
COPY --from=slides /out /srv/slides
# Project splash page (site/), served at / on HASHU_EXTRA_DOMAINS only (see Caddyfile).
COPY site/ /srv/site/
COPY --from=redeem /out /srv/site/redeem/
