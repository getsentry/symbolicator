# CLI
FROM debian:bookworm-slim AS sentry-cli

RUN apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sL https://sentry.io/get-cli/ | sh

# Image with cargo-chef as base image to our builder
FROM rust:slim-bookworm AS symbolicator-chef

WORKDIR /work
RUN cargo install cargo-chef --locked

# Image that generates cargo-check recipe
FROM symbolicator-chef AS symbolicator-planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Image that builds the final symbolicator
FROM symbolicator-chef AS symbolicator-build

RUN apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends build-essential ca-certificates curl libssl-dev pkg-config git zip clang mold \
    # below required for sentry-native
    cmake clang libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*

ARG SYMBOLICATOR_FEATURES=symbolicator-crash
ENV SYMBOLICATOR_FEATURES=${SYMBOLICATOR_FEATURES}
ENV RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=/usr/bin/mold"

COPY --from=symbolicator-planner /work/recipe.json recipe.json

# Build only the dependencies identified in the `symbolicator-planner` image
RUN cargo chef cook --profile release-lto --features=${SYMBOLICATOR_FEATURES} --recipe-path recipe.json

COPY . .
RUN git update-index --skip-worktree $(git status | grep deleted | awk '{print $2}')
RUN cargo build -p symbolicator --profile release-lto --features=${SYMBOLICATOR_FEATURES}
RUN cp target/release-lto/symbolicator target/release-lto/symbolicator.debug \
    && objcopy --strip-debug --strip-unneeded target/release-lto/symbolicator \
    && objcopy --add-gnu-debuglink target/release-lto/symbolicator target/release-lto/symbolicator.debug \
    && cp ./target/release-lto/symbolicator /usr/local/bin \
    && zip /opt/symbolicator-debug.zip target/release-lto/symbolicator.debug

COPY --from=sentry-cli /usr/local/bin/sentry-cli /bin/sentry-cli
RUN sentry-cli --version \
    && SOURCE_BUNDLE="$(sentry-cli difutil bundle-sources ./target/release-lto/symbolicator.debug)" \
    && mv "$SOURCE_BUNDLE" /opt/symbolicator.src.zip

#############################################
# Copy the compiled binary to a clean image #
#############################################

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends openssl ca-certificates gosu curl cabextract \
    && rm -rf /var/lib/apt/lists/*

ENV \
    SYMBOLICATOR_UID=10021 \
    SYMBOLICATOR_GID=10021

# Create a new user and group with fixed uid/gid
RUN groupadd --system symbolicator --gid $SYMBOLICATOR_GID \
    && useradd --system --gid symbolicator --uid $SYMBOLICATOR_UID symbolicator

VOLUME ["/data"]
RUN mkdir -p /etc/symbolicator /data && \
    chown symbolicator:symbolicator /etc/symbolicator /data

EXPOSE 3021

COPY --from=symbolicator-build /usr/local/bin/symbolicator /bin

COPY ./docker-entrypoint.sh /
ENTRYPOINT ["/bin/bash", "/docker-entrypoint.sh"]
