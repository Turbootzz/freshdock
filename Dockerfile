# syntax=docker/dockerfile:1
#
# Multi-arch image assembled from the prebuilt static-musl binaries produced by
# .github/workflows/release.yml. The binary is statically linked, so no libc is
# needed; distroless/static is used (over bare `scratch`) only for the CA root
# bundle the registry HTTPS calls require, and it runs as root so a mounted
# /var/run/docker.sock stays accessible.
#
# buildx sets TARGETARCH / TARGETVARIANT per platform; they select the matching
# binary staged as dist/freshdock-<arch><variant>:
#   linux/amd64  -> freshdock-amd64
#   linux/arm64  -> freshdock-arm64
#   linux/arm/v7 -> freshdock-armv7
FROM gcr.io/distroless/static-debian12:latest

ARG TARGETARCH
ARG TARGETVARIANT

LABEL org.opencontainers.image.source="https://github.com/Turbootzz/freshdock" \
      org.opencontainers.image.description="freshdock — a modern Rust-based Docker container auto-updater (Watchtower successor)" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY dist/freshdock-${TARGETARCH}${TARGETVARIANT} /usr/local/bin/freshdock

ENTRYPOINT ["/usr/local/bin/freshdock"]
