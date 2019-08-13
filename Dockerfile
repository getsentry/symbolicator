FROM rust:slim-buster AS symbolicator-build

WORKDIR /work

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential libssl-dev pkg-config git \
    && rm -rf /var/lib/apt/lists/*

# Build only dependencies to speed up subsequent builds
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src \
    && echo "fn main() {}" > src/main.rs \
    && RUSTFLAGS=-g cargo build --release --locked

COPY src ./src/
COPY .git ./.git/
RUN RUSTFLAGS=-g cargo build --release --locked
RUN cp ./target/release/symbolicator /usr/local/bin

#############################################
# Copy the compiled binary to a clean image #
FROM debian:buster-slim
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
ONBUILD COPY --chown=symbolicator:symbolicator config.yml /etc/symbolicator/config.yml

EXPOSE 3021

COPY --from=symbolicator-build --chown=symbolicator:symbolicator /usr/local/bin/symbolicator /bin

COPY ./docker-entrypoint.sh /
ENTRYPOINT ["/bin/bash", "/docker-entrypoint.sh"]
