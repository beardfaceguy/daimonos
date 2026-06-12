# syntax=docker/dockerfile:1.7
#
# Daimonos — agent-optimized MCP server.
#
# This image exists primarily so introspection services (e.g. Glama) can
# spin up daimonos and verify it answers an MCP `initialize` request. The
# image is NOT the recommended way to deploy daimonos — for editor
# integration, use the native binary from GitHub Releases.
#
# Build:   docker build -t daimonos .
# Run:     docker run --rm -i -v "$PWD":/workspace daimonos
#
# The container reads MCP requests from stdin and writes responses to
# stdout, so `-i` is required. Mount your project at /workspace.
#
# Glama releases: add a build step `ENV DAIMONOS_MCP_FULL_SCHEMAS=1` so
# list_tools exposes full JSON Schemas for Terse-tier tools (TDQS scoring).

# ---- build stage ----
FROM rust:1-bookworm AS build
WORKDIR /src

# Copy only the files cargo needs to resolve and build, so unrelated
# changes (docs, tests, distro/, etc.) don't invalidate the cache.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked \
 && strip target/release/daimonos

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime

# git is needed for daimonos's git tool plugin to auto-register.
# ca-certificates is needed for any HTTPS calls the agent may make.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/daimonos /usr/local/bin/daimonos

WORKDIR /workspace
ENTRYPOINT ["daimonos", "--mcp", "-w", "/workspace"]
