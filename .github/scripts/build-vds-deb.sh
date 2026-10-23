#!/bin/bash
# Builds libvds0 and libvds-dev inside a pxvirt builder container.
# Usage: build-vds-deb.sh OUTDIR
set -euxo pipefail
OUT=${1:?output directory}
export DEBIAN_FRONTEND=noninteractive
# The pxvirt repositories changed their Origin/Label after the images were built.
apt-get update --allow-releaseinfo-change
apt-get install -y --no-install-recommends curl ca-certificates nasm g++ pkg-config git

# The distributions' rustc is older than vds's rust-version, so use rustup.
if ! command -v rustup >/dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none --no-modify-path
fi
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
command -v cargo-deb >/dev/null || cargo install cargo-deb --locked

cargo deb -p vds-capi
cargo deb -p vds-capi --variant dev --no-build
mkdir -p "$OUT"
cp target/debian/libvds0_*.deb target/debian/libvds-dev_*.deb "$OUT"/
ls -la "$OUT"
