FROM rust:slim-stretch AS symbolicator-build

WORKDIR /work

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential libssl-dev pkg-config git \
    && rm -rf /var/lib/apt/lists/*

# Build only dependencies to speed up subsequent builds
ADD Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo "fn main() {}" > src/main.rs \
    && RUSTFLAGS=-g cargo build --release --locked

COPY . .
RUN RUSTFLAGS=-g cargo build --release --locked
RUN cp ./target/release/symbolicator /usr/local/bin

# Copy the compiled binary to a clean image
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

RUN mkdir /etc/symbolicator \
    && chown symbolicator:symbolicator /etc/symbolicator
VOLUME ["/etc/symbolicator"]

EXPOSE 3021

COPY --from=symbolicator-build /usr/local/bin/symbolicator /bin
# Smoke test
RUN symbolicator --version && symbolicator --help

COPY ./docker-entrypoint.sh /
ENTRYPOINT ["/bin/bash", "/docker-entrypoint.sh"]
