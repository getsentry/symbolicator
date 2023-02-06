#!/bin/bash

# DIF uploading is skipped if SENTRY_AUTH_TOKEN isn't set; we only want this
# running on pushes to master, not during GHA (which builds ghcr.io images for open source).

if ! [[ "$SENTRY_AUTH_TOKEN" ]]; then
    echo "Skipping DIF building and uploading; SENTRY_AUTH_TOKEN not set."
    exit 0
fi

SOURCE_BUNDLE="$(sentry-cli difutil bundle-sources ./target/release/symbolicator.debug)"
SENTRY_URL="https://sentry.my.sentry.io/"

sentry-cli --version
sentry-cli upload-dif -o sentry -p symbolicator "$SOURCE_BUNDLE" /opt/symbolicator-debug.zip
