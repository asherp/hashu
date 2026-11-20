#!/bin/sh
# Check the required settings, then write the optional MDK demo routes and
# start Caddy. Nothing here defaults to a particular deployment: an unset
# required variable stops the container with its name.
set -eu

fail() { printf 'hashu-caddy: %s\n' "$*" >&2; exit 1; }

[ -n "${HASHU_DOMAIN:-}" ] || fail "HASHU_DOMAIN is not set: the public DNS name of this mint (e.g. mint.example.com), pointing at this machine"
case "$HASHU_DOMAIN" in
  *://*|*/*) fail "HASHU_DOMAIN must be a bare host name like mint.example.com (got $HASHU_DOMAIN)" ;;
esac

GEN=/etc/caddy/generated
mkdir -p "$GEN"
if [ -n "${HASHU_MDK_DOMAIN:-}" ]; then
  # MDK dashboard demo: /mdk on the mint redirects to its own host, which
  # serve.js in the mdk container answers (compose profile "mdk").
  cat > "$GEN/mdk-routes.caddy" <<ROUTES
redir /mdk https://{\$HASHU_MDK_DOMAIN}/ 302
redir /mdk/ https://{\$HASHU_MDK_DOMAIN}/ 302
ROUTES
  cat > "$GEN/mdk-site.caddy" <<SITE
{\$HASHU_MDK_DOMAIN} {
	encode gzip
	reverse_proxy mdk:3030
}
SITE
else
  echo "# MDK demo off (HASHU_MDK_DOMAIN unset)" > "$GEN/mdk-routes.caddy"
  echo "# MDK demo off (HASHU_MDK_DOMAIN unset)" > "$GEN/mdk-site.caddy"
fi

if [ -n "${HASHU_APP_DOMAIN:-}" ]; then
  case "$HASHU_APP_DOMAIN" in
    *://*|*/*) fail "HASHU_APP_DOMAIN must be a bare host name like app.example.com (got $HASHU_APP_DOMAIN)" ;;
  esac
  # hashu.me wallet PWA, built into this image at /srv/app. A single-page
  # app: unknown paths get index.html. The service worker and manifest
  # must not be cached, or installed wallets never see an update.
  cat > "$GEN/app-site.caddy" <<SITE
{\$HASHU_APP_DOMAIN} {
	import security_headers
	encode gzip
	root * /srv/app
	@fresh path /sw.js /manifest.json /index.html /
	header @fresh Cache-Control "no-cache"
	try_files {path} /index.html
	file_server
}
SITE
else
  echo "# hashu.me wallet off (HASHU_APP_DOMAIN unset)" > "$GEN/app-site.caddy"
fi

exec "$@"
