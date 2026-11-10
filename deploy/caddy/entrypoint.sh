#!/bin/sh
# Check the required settings, then write the optional MDK demo routes and
# start Caddy. Nothing here defaults to a particular deployment: an unset
# required variable stops the container with its name.
set -eu

fail() { printf 'hashu-caddy: %s\n' "$*" >&2; exit 1; }

[ -n "${HASHU_DOMAIN:-}" ] || fail "HASHU_DOMAIN is not set: the public DNS name of this mint (e.g. mint.example.com), pointing at this machine"
[ -n "${OPERATOR_PASSWORD_HASH:-}" ] || fail "OPERATOR_PASSWORD_HASH is not set: run 'caddy hash-password' and put the hash in .env"
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

exec "$@"
