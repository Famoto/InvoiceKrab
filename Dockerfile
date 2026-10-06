# Builds krab-cli and krab-server as static musl binaries in scratch images.
#
#   docker build --target server -t krab-server .        # HTTP service (default)
#   docker build --target cli    -t krab-cli .           # CLI
#
# The image compiles the mappings it is built with. To compile your own,
# keep them in the build context and name their directory (relative to the
# context root) with a build argument:
#
#   docker build --build-arg KRAB_CONFIG_DIR=my-config -t my-krab-server .
#
# Multi-arch: `docker buildx build --platform linux/amd64,linux/arm64 ...`.

FROM rust:1-slim-bookworm AS builder
ARG TARGETARCH
# Context-relative directory of your own mappings; empty builds config/.
ARG KRAB_CONFIG_DIR=
WORKDIR /app
RUN case "$TARGETARCH" in \
        amd64|"") echo x86_64-unknown-linux-musl ;; \
        arm64) echo aarch64-unknown-linux-musl ;; \
        *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac > /rust-target \
    && rustup target add "$(cat /rust-target)"
COPY . .
# One compile of the workspace produces both binaries. The cache mounts keep
# the registry and compiled dependencies across builds, so changing a mapping
# recompiles the workspace crates only.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    export KRAB_CONFIG_DIR="${KRAB_CONFIG_DIR:+/app/$KRAB_CONFIG_DIR}" \
    && cargo build --release --locked --target "$(cat /rust-target)" -p einvoice-interfaces \
    && mkdir /out \
    && cp "target/$(cat /rust-target)/release/krab-cli" "target/$(cat /rust-target)/release/krab-server" /out/

FROM scratch AS cli
COPY --from=builder /out/krab-cli /krab-cli
USER 65534
ENTRYPOINT ["/krab-cli"]

FROM scratch AS server
COPY --from=builder /out/krab-server /krab-server
USER 65534
EXPOSE 8080
# Runtime configuration — override with `docker run -e`.
ENV KRAB_ADDR=0.0.0.0:8080
# scratch has no curl; the binary doubles as its own probe client.
HEALTHCHECK --interval=30s --timeout=5s --start-period=2s \
    CMD ["/krab-server", "--healthcheck"]
ENTRYPOINT ["/krab-server"]
