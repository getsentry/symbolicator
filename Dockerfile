FROM us-docker.pkg.dev/sentryio/dhi-mirror/static:20250419-glibc-debian13 AS builder

FROM us-docker.pkg.dev/sentryio/dhi-mirror/static:20250419-glibc-debian13

# The base image does not have a shell available, which means we cannot
# `mkdir` the volume directories. As an alternative we can use the empty
# home directory instead.
COPY --from=builder --chown=nonroot:nonroot /home/nonroot /etc/symbolicator
COPY --from=builder --chown=nonroot:nonroot /home/nonroot /data

VOLUME ["/etc/symbolicator", "/data"]
EXPOSE 3021

ARG TARGETPLATFORM

ARG BINARY=./binaries/$TARGETPLATFORM/symbolicator
COPY --chmod=0755 ${BINARY} /bin/symbolicator

ENTRYPOINT ["/bin/symbolicator"]
