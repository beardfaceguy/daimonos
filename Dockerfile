#
# Daimonos — agent-optimized MCP server.
#
# This image exists primarily so introspection services (e.g. Glama) can
# spin up daimonos and verify it answers an MCP `initialize` request. The
# image is NOT the recommended way to deploy daimonos — for editor
# integration, use the native binary from GitHub Releases.
#
# Build a pinned local image (no registry push). See docs/configuration.md.
# Supply VERSION from Cargo.toml and SOURCE_COMMIT from the archived source.
# Run:     docker run --rm -i -v "$PWD":/workspace daimonos
#
# The container reads MCP requests from stdin and writes responses to
# stdout, so `-i` is required. With no arguments the workspace is /workspace.
#
# Set DAIMONOS_MCP_FULL_SCHEMAS=1 at runtime to list full schemas.

# ---- build stage ----
FROM rust:1.96.0-bookworm AS build
WORKDIR /src

# Copy only the files cargo needs to resolve and build, so unrelated
# changes (docs, tests, distro/, etc.) don't invalidate the cache.
COPY Cargo.toml Cargo.lock daimonos.default.toml ./
COPY prompts ./prompts
COPY src ./src

ARG SOURCE_COMMIT
ARG VERSION
RUN test "${#SOURCE_COMMIT}" = 40 && ! printf '%s' "$SOURCE_COMMIT" | grep -q '[^0-9a-f]' \
 && test -n "$VERSION" \
 && DAIMONOS_SOURCE_COMMIT="$SOURCE_COMMIT" cargo build --release --locked -j 1 \
 && strip target/release/daimonos \
 && target/release/daimonos --version | grep -Fx "daimonos $VERSION" \
 && target/release/daimonos --version | grep -Fx "source_commit $SOURCE_COMMIT"

# ---- runtime stage ----
FROM debian:trixie-slim AS runtime
ARG SOURCE_COMMIT
ARG VERSION
LABEL org.opencontainers.image.revision="$SOURCE_COMMIT" \
      org.opencontainers.image.version="$VERSION"

# git is needed for daimonos's git tool plugin to auto-register.
# ca-certificates is needed for any HTTPS calls the agent may make.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && install -d -m 0700 -o 65532 -g 65532 /home/daimonos

COPY --from=build /src/target/release/daimonos /usr/local/bin/daimonos

RUN install -d -m 0755 -o 65532 -g 65532 /workspace
WORKDIR /workspace
ENV HOME=/home/daimonos
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/daimonos"]
CMD ["mcp"]
