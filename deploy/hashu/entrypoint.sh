#!/bin/sh
# Drop empty HASHU_* / PHOENIXD_* variables before starting hashu. Compose
# passes `${VAR:-}` through as an empty string, and clap treats an empty env
# value as a (malformed) value, e.g. `--usdt-per-thh ''`. Unset = not set.
for v in $(env | sed -n 's/^\(HASHU_[A-Z0-9_]*\)=$/\1/p; s/^\(PHOENIXD_[A-Z0-9_]*\)=$/\1/p'); do
  unset "$v"
done
exec hashu "$@"
