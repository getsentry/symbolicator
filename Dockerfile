# CLI
FROM getsentry/sentry-cli:2 AS sentry-cli

# Image with cargo-chef as base image to our builder
FROM rust:slim-bullseye AS symbolicator-chef

WORKDIR /work
RUN cargo install cargo-chef --locked

# Image that generates cargo-check recipe
FROM symbolicator-chef AS symbolicator-planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Image that builds the final symbolicator
FROM symbolicator-chef AS symbolicator-build

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential ca-certificates curl libssl-dev pkg-config git zip \
    # below required for sentry-native
    cmake clang libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*

ARG SYMBOLICATOR_FEATURES=symbolicator-crash
ENV SYMBOLICATOR_FEATURES=${SYMBOLICATOR_FEATURES}

COPY --from=symbolicator-planner /work/recipe.json recipe.json

# Build only the dependencies identified in the `symbolicator-planner` image
RUN cargo chef cook --release --features=${SYMBOLICATOR_FEATURES} --recipe-path recipe.json

RUN rustup toolchain install nightly

COPY . .
RUN git update-index --skip-worktree $(git status | grep deleted | awk '{print $2}')
RUN cargo +nightly run -Z build-std --target x86_64-unknown-linux-gnu
RUN RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --features=${SYMBOLICATOR_FEATURES}
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
    && apt-get install -y --no-install-recommends openssl ca-certificates gosu curl cabextract \
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
