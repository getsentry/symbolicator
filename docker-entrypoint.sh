#!/usr/bin/env bash
set -eu

if [ "$(id -u)" == "0" ]; then
  # Prepare default data directory
  # WARNING(BYK): This should be done for the cache_dir mounted by any means, otherwise it is not
  #               guaranteed to be writable, causing a runtime failure during downloads.
  chown symbolicator:symbolicator /data

  exec gosu symbolicator /bin/symbolicator "$@"
else
  exec /bin/symbolicator "$@"
fi
