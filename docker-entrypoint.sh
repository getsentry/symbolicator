#!/usr/bin/env bash
set -eu

if [ "$(id -u)" == "0" ]; then
  exec gosu symbolicator /bin/symbolicator "$@"
else
  exec /bin/symbolicator "$@"
fi
