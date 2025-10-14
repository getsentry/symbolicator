FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=gcr.io/distroless/cc-debian12:nonroot --chown=nonroot:nonroot /home/nonroot /etc/symbolicator
COPY --from=gcr.io/distroless/cc-debian12:nonroot --chown=nonroot:nonroot /home/nonroot /data

VOLUME ["/etc/symbolicator", "/data"]
EXPOSE 3021

ARG TARGETPLATFORM

ARG BINARY=./binaries/$TARGETPLATFORM/symbolicator
COPY ${BINARY} /bin/symbolicator

ENTRYPOINT ["/bin/symbolicator"]
