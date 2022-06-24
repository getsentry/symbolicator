FROM getsentry/sentry-cli:1 AS sentry-cli
FROM debian:stretch-slim AS symbolicator-build

WORKDIR /work

# Hooray for running with a totally outdated debian image
RUN echo 'deb http://deb.debian.org/debian stretch-backports main' > /etc/apt/sources.list.d/cmake-backports.list
RUN echo 'deb http://deb.debian.org/debian stretch-backports-sloppy main' >> /etc/apt/sources.list.d/cmake-backports.list

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential ca-certificates curl libssl-dev pkg-config git zip \
    # below required for sentry-native
    clang-11 cmake/stretch-backports libarchive13/stretch-backports-sloppy libuv1/stretch-backports libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Because the image has gcc-6 as default compiler, and 3.8 as default clang. We are at 11 and 14 respectively. Let that sink in.
ENV CC=clang-11 CXX=clang++-11

ENV CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

# We should really depend on the rust:slim-buster images again as this
# will automatically upgrade our Rust toolchains when a new one is
# released.  But while we can't: bump versions manually.
ENV RUST_TOOLCHAIN=1.61.0

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain $RUST_TOOLCHAIN

ARG SYMBOLICATOR_FEATURES=symbolicator-crash
ENV SYMBOLICATOR_FEATURES=${SYMBOLICATOR_FEATURES}

# Build only dependencies to speed up subsequent builds
COPY Cargo.toml Cargo.lock ./

COPY crates/symbolicator/build.rs crates/symbolicator/Cargo.toml crates/symbolicator/
COPY crates/symbolicator-crash/build.rs crates/symbolicator-crash/Cargo.toml crates/symbolicator-crash/

# Build without --locked.
#
# CI already builds with --locked so we are sure exactly which
# dependencies we are building with.  However because we do not copy
# in all the crates into the build the workspace-wide Cargo.lock file
# will get some unused dependencies pruned during this build.
RUN mkdir -p crates/symbolicator/src \
    && echo "fn main() {}" > crates/symbolicator/src/main.rs \
    && mkdir -p crates/symbolicator-crash/src \
    && echo "pub fn main() {}" > crates/symbolicator-crash/src/lib.rs \
    && cargo build --release

COPY crates/symbolicator/src crates/symbolicator/src/
COPY crates/symbolicator-crash/src crates/symbolicator-crash/src/
COPY crates/symbolicator-crash/sentry-native crates/symbolicator-crash/sentry-native/
COPY .git ./.git/
# Ignore missing (deleted) files for dirty-check in `git describe` call for version
# This is a bit hacky because it ignores *all* deleted files, not just the ones we skipped in Docker
RUN git update-index --skip-worktree $(git status | grep deleted | awk '{print $2}')
RUN cargo build --release --features=${SYMBOLICATOR_FEATURES}
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
