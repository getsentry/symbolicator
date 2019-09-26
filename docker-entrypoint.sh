#!/usr/bin/env bash
set -eu

if [ "$(id -u)" == "0" ]; then
  # Prepare default data directory
  mkdir -p /data
  chown symbolicator:symbolicator /data

  exec gosu symbolicator /bin/symbolicator "$@"
else
  exec /bin/symbolicator "$@"
fi
