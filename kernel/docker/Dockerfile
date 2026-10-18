# Build environment for the GB350 mainline kernel (little-endian MIPS32r2).
FROM debian:trixie-slim
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        build-essential bc bison flex libssl-dev libelf-dev cpio kmod rsync xz-utils \
        ca-certificates curl python3 gcc-mipsel-linux-gnu u-boot-tools device-tree-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
