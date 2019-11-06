FROM getsentry/sentry-cli:1 AS sentry-cli
FROM rust:slim-stretch AS symbolicator-build

WORKDIR /work

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential libssl-dev pkg-config git zip \
    && rm -rf /var/lib/apt/lists/*

# Build only dependencies to speed up subsequent builds
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src \
    && echo "fn main() {}" > src/main.rs \
    && RUSTFLAGS=-g cargo build --release --locked

COPY src ./src/
COPY .git ./.git/
# Ignore missing (deleted) files for dirty-check in `git describe` call for version
# This is a bit hacky because it ignores *all* deleted files, not just the ones we skipped in Docker
RUN git update-index --skip-worktree $(git status | grep deleted | awk '{print $2}')
RUN RUSTFLAGS=-g cargo build --release --locked
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

FROM debian:stretch-slim
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
