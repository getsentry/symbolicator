FROM mcr.microsoft.com/vscode/devcontainers/base:bullseye

RUN apt-get update -yq \
    && apt-get install -yq --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    git \
    libssl-dev \
    lld \
    pkg-config \
    zip \
    # extra debug tools
    valgrind \
    # below required for sentry-native
    cmake \
    clang \
    libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*

USER vscode

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y

# Add .cargo/bin to PATH
ENV PATH="/home/vscode/.cargo/bin:${PATH}"

# Check cargo is visible
RUN cargo install sccache
