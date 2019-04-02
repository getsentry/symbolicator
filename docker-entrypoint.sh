#!/usr/bin/env bash
set -eu

if [ "$(id -u)" == "0" ]; then
  echo '127.0.0.1 api.local.sentry.io' >> /etc/hosts  # For tests
  exec gosu symbolicator /bin/symbolicator "$@"
else
  exec /bin/symbolicator "$@"
fi
