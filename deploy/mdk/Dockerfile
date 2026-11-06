# MDK demo stack for the live site: the Hashu Heater app, the heater Worker
# with its mock heater, two accept-everything fake pools, the MDK Kernel and
# Gateway, and the built MDK dashboard (served by serve.js on :3030).
# Build context is the repo root (see deploy/docker-compose.yml).
#
# Everything here is mock: the heater's shares are fake, so it talks only to
# the fake pools and to the separate demo mint (service mdk-hashu), never to
# the live mint or a real pool.

FROM node:24-bookworm-slim
RUN npm install -g @tetherto/mdk-cli@0.13.1 --no-audit --no-fund
WORKDIR /app/mdk
COPY mdk/ ./
RUN rm -rf node_modules apps/dashboard .mdk apps/hashu/ui-src/node_modules \
 && npm install --no-audit --no-fund
# Point the app at the demo mint on the compose network. The redeem pool is
# dialed by that mint's proxy, so it names this container (mdk), not loopback.
RUN sed -i \
      -e 's#^\(        \)homePool: { url: "stratum+tcp://127.0.0.1:3340"#\1hashuControlUrl: "http://mdk-hashu:3334"\n\1proxyPool: { url: "stratum+tcp://mdk-hashu:3333", user: "hashu.heater", pass: "x" }\n&#' \
      -e 's#redeemPool: { url: "stratum+tcp://127.0.0.1:3341"#redeemPool: { url: "stratum+tcp://mdk:3341"#' \
      mdk.yaml \
 && grep -q 'mdk-hashu:3334' mdk.yaml && grep -q 'mdk:3341' mdk.yaml
# The dashboard (fetched from MDK's GitHub template), built with the Hashu app's UI.
RUN mdk create dashboard \
 && cd apps/dashboard \
 && MDK_DASHBOARD_APPS='[{"name":"hashu","id":"io.hashu.heater","version":"0.1.0","displayName":"Hashu Heater","apiBase":"/apps/hashu","dir":"/app/mdk/apps/hashu"}]' \
    npm run build \
 && test -f dist/app-ui/hashu/index.js
COPY deploy/mdk/serve.js /app/deploy/mdk/serve.js
COPY --chmod=0755 deploy/mdk/entrypoint.sh /usr/local/bin/mdk-entrypoint
ENV MDK_DASHBOARD_DIST=/app/mdk/apps/dashboard/dist
# 3030 dashboard + API (for Caddy), 3340/3341 fake pools (for mdk-hashu).
EXPOSE 3030 3340 3341
ENTRYPOINT ["mdk-entrypoint"]
