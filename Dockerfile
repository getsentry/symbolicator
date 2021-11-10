FROM getsentry/sentry-cli:1 AS sentry-cli
FROM rust:slim-bullseye AS symbolicator-build

WORKDIR /work

# Build only dependencies to speed up subsequent builds
COPY Cargo.toml Cargo.lock ./

COPY crates/symbolicator/build.rs crates/symbolicator/Cargo.toml crates/symbolicator/

RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl-dev pkg-config g++ git zip \
    && rm -rf /var/lib/apt/lists/*
# Build without --locked.
#
# CI already builds with --locked so we are sure exactly which
# dependencies we are building with.  However because we do not copy
# in all the crates into the build the workspace-wide Cargo.lock file
# will get some unused dependencies pruned during this build.
RUN mkdir -p crates/symbolicator/src \
    && echo "fn main() {}" > crates/symbolicator/src/main.rs \
    && cargo build --release

COPY crates/symbolicator/src crates/symbolicator/src/
COPY .git ./.git/
# Ignore missing (deleted) files for dirty-check in `git describe` call for version
# This is a bit hacky because it ignores *all* deleted files, not just the ones we skipped in Docker
RUN git update-index --skip-worktree $(git status | grep deleted | awk '{print $2}')
RUN cargo build --release
RUN objcopy --only-keep-debug target/release/symbolicator target/release/symbolicator.debug \
    && objcopy --strip-debug --strip-unneeded target/release/symbolicator \
    && objcopy --add-gnu-debuglink target/release/symbolicator target/release/symbolicator.debug \
    && cp ./target/release/symbolicator /usr/local/bin \
    && zip /opt/symbolicator-debug.zip target/release/symbolicator.debug

COPY --from=sentry-cli /bin/sentry-cli /bin/sentry-cli
RUN sentry-cli --version \
    && SOURCE_BUNDLE="$(sentry-cli difutil bundle-sources ./target/release/symbolicator.debug)" \
    && mv "$SOURCE_BUNDLE" /opt/symbolicator.src.zip

#############################################
# Copy the compiled binary to a clean image #
#############################################

FROM debian:bullseye-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends openssl ca-certificates gosu cabextract \
    && rm -rf /var/lib/apt/lists/*

ENV \
    SYMBOLICATOR_UID=10021 \
    SYMBOLICATOR_GID=10021

# Create a new user and group with fixed uid/gid
RUN groupadd --system symbolicator --gid $SYMBOLICATOR_GID \
    && useradd --system --gid symbolicator --uid $SYMBOLICATOR_UID symbolicator

VOLUME ["/data"]
RUN mkdir /etc/symbolicator && \
    chown symbolicator:symbolicator /etc/symbolicator /data

EXPOSE 3021

COPY --from=symbolicator-build /usr/local/bin/symbolicator /bin
COPY --from=symbolicator-build /opt/symbolicator-debug.zip /opt/symbolicator.src.zip /opt/

COPY ./docker-entrypoint.sh /
ENTRYPOINT ["/bin/bash", "/docker-entrypoint.sh"]
