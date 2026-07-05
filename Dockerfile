FROM rust:1-slim-bookworm AS builder
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /app
COPY . .
# One compile of the workspace produces both binaries.
RUN cargo build --release --target x86_64-unknown-linux-musl -p einvoice-interfaces

FROM scratch AS cli
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/krab-cli /krab-cli
USER 65534
ENTRYPOINT ["/krab-cli"]

# docker build --target server -t krab-server .
FROM scratch AS server
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/krab-server /krab-server
USER 65534
EXPOSE 8080
# Runtime configuration — override with `docker run -e`.
ENV KRAB_ADDR=0.0.0.0:8080
# scratch has no curl; the binary doubles as its own probe client.
HEALTHCHECK --interval=30s --timeout=5s --start-period=2s \
    CMD ["/krab-server", "--healthcheck"]
ENTRYPOINT ["/krab-server"]
