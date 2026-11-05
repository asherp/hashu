# Thin wrapper over ACINQ's phoenixd image, adapted from fathom-x/overpay's
# docker/phoenixd. The entrypoint can import an existing wallet seed on first
# boot, then launches phoenixd. Its HTTP API is only reachable on the compose
# network (no host port), so only the Hashu mint can talk to it.
ARG PHOENIXD_VERSION=0.9.1
FROM acinq/phoenixd:${PHOENIXD_VERSION}
COPY --chmod=0755 entrypoint.sh /usr/local/bin/phoenixd-entrypoint.sh
ENTRYPOINT ["/usr/local/bin/phoenixd-entrypoint.sh"]
