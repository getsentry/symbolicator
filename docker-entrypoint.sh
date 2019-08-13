#!/usr/bin/env bash
set -eu

if [ "$(id -u)" == "0" ]; then
  # Prepare default data and config directories
  chown -R symbolicator:symbolicator /etc/symbolicator
  chown symbolicator:symbolicator /data

  exec gosu symbolicator /bin/symbolicator "$@"
else
  exec /bin/symbolicator "$@"
fi
