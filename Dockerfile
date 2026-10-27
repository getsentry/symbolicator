# The hardened base image only ships glibc and libgcc_s, but symbolicator also
# links against OpenSSL (and its zlib/zstd deps) and libstdc++. Take those from
# distroless Debian 13, which matches the base image's distro and glibc.
FROM gcr.io/distroless/cc-debian13:nonroot AS libs

FROM us-docker.pkg.dev/sentryio/dhi-mirror/static:20250419-glibc-debian13 AS base

FROM base

COPY --from=libs \
    /usr/lib/*-linux-gnu/libssl.so.3 \
    /usr/lib/*-linux-gnu/libcrypto.so.3 \
    /usr/lib/*-linux-gnu/libz.so.1 \
    /usr/lib/*-linux-gnu/libzstd.so.1 \
    /usr/lib/*-linux-gnu/libstdc++.so.6 \
    /usr/lib/

# The base image does not have a shell available, which means we cannot
# `mkdir` the volume directories. As an alternative we can use the empty
# home directory instead.
COPY --from=base --chown=nonroot:nonroot /home/nonroot /etc/symbolicator
COPY --from=base --chown=nonroot:nonroot /home/nonroot /data

VOLUME ["/etc/symbolicator", "/data"]
EXPOSE 3021

ARG TARGETPLATFORM

ARG BINARY=./binaries/$TARGETPLATFORM/symbolicator
COPY --chmod=0755 ${BINARY} /bin/symbolicator

ENTRYPOINT ["/bin/symbolicator"]
