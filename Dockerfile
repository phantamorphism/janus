# syntax=docker/dockerfile:1

# ---- UI ---------------------------------------------------------------------
FROM node:24-slim AS ui
WORKDIR /src/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build

# ---- Rust binaries ----------------------------------------------------------
FROM rust:1-bookworm AS build
# janus-core/build.rs compiles proto/janus.proto and needs protoc. The image's
# own toolchain is used as-is (rust-toolchain.toml would ask rustup to fetch
# components the build does not need).
ENV RUSTUP_TOOLCHAIN=stable
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY proto proto
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p janus-daemon -p janus-cli \
 && mkdir -p /out \
 && cp target/release/janus-daemon target/release/janus /out/

# ---- Runtime ----------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 janus \
 && useradd --system --uid 10001 --gid janus --home-dir /var/lib/janus --shell /usr/sbin/nologin janus \
 && install -d -o janus -g janus -m 0700 /var/lib/janus \
 && install -d -m 0755 /etc/janus

COPY --from=build /out/janus-daemon /out/janus /usr/local/bin/
# The daemon looks for UI assets at <exe_dir>/ui, then <exe_dir>/../share/janus/ui.
COPY --from=ui /src/ui/dist /usr/local/share/janus/ui
COPY scripts/ci/docker/janus.toml /etc/janus/janus.toml

ENV JANUS_DATA_DIR=/var/lib/janus
VOLUME /var/lib/janus
# Data plane. Add [admin] / [ui] sections to your own janus.toml to expose more.
EXPOSE 7600
USER janus:janus
ENTRYPOINT ["janus-daemon"]
CMD ["--config", "/etc/janus/janus.toml"]
