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

# hashu.me wallet (github.com/asherp/hashu.me, a fork of Cashu.me): a
# static PWA served at $HASHU_APP_DOMAIN when that is set. HASHU_APP_REF is
# a branch, tag or commit; the images workflow passes the wallet's current
# main commit so a new wallet commit busts this layer's cache.
FROM node:22-bookworm AS wallet
ARG HASHU_APP_REPO=https://github.com/asherp/hashu.me.git
ARG HASHU_APP_REF=main
WORKDIR /wallet
RUN git init -q . \
 && git remote add origin "$HASHU_APP_REPO" \
 && git fetch -q --depth 1 origin "$HASHU_APP_REF" \
 && git checkout -q FETCH_HEAD
RUN npm ci --no-audit --no-fund
RUN npx quasar build -m pwa
# Hashu Operator (hashu.me issue #42): the same repo built as a second app
# (its own manifest and storage) into dist/operator, served at
# $HASHU_OPERATOR_APP_DOMAIN. A wallet ref from before the operator app
# builds nothing here, and that host then answers 404.
RUN if grep -q HASHU_APP quasar.config.js; then \
      HASHU_APP=operator npx quasar build -m pwa; \
    else mkdir -p dist/operator; fi

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
COPY --from=wallet /wallet/dist/pwa /srv/app
COPY --from=wallet /wallet/dist/operator /srv/operator
COPY --from=slides /out /srv/slides
# Project splash page (site/), served at / on HASHU_EXTRA_DOMAINS only (see Caddyfile).
COPY site/ /srv/site/
COPY --from=redeem /out /srv/site/redeem/
