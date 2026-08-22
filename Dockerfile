# Pinned to the same Debian release as the runtime stage below — a newer
# builder (e.g. plain `rust:1-slim`, which tracks whatever Debian release is
# current) links against a glibc the older runtime image can't satisfy
# ("GLIBC_2.38/2.39 not found"), hit live deploying this.
FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# Cache dependency compilation separately from source changes. Workspace
# member `imou-vision` needs its own Cargo.toml present before `cargo build`
# will resolve the graph, so it gets the same dummy-source treatment.
COPY Cargo.toml Cargo.lock ./
COPY crates/imou-vision/Cargo.toml crates/imou-vision/Cargo.toml
RUN mkdir -p src crates/imou-vision/src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '// dummy for dependency caching' > crates/imou-vision/src/lib.rs \
    && cargo build --release --bin imou-cli \
    && rm -rf src crates/imou-vision/src

COPY src ./src
COPY crates/imou-vision/src ./crates/imou-vision/src
RUN touch src/main.rs crates/imou-vision/src/lib.rs && cargo build --release --bin imou-cli

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/imou-cli /usr/local/bin/imou-cli
WORKDIR /app
ENTRYPOINT ["imou-cli"]
