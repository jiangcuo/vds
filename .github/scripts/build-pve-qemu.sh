#!/bin/bash
# Builds pve-qemu-kvm with -rdp inside a pxvirt builder container, the standard way:
#   apt update && mk-build-deps --install --remove && make deb
# Usage: build-pve-qemu.sh PVE_QEMU_DIR LIBVDS_DEB_DIR
set -euxo pipefail
SRC=${1:?pve-qemu checkout}
DEBS=${2:?directory with libvds0/libvds-dev}
export DEBIAN_FRONTEND=noninteractive
APT_UPDATE="apt-get update --allow-releaseinfo-change"

$APT_UPDATE
apt-get install -y --no-install-recommends python3-venv git
apt-get install -y "$DEBS"/libvds0_*.deb "$DEBS"/libvds-dev_*.deb

cd "$SRC"
git config --global --add safe.directory '*'
# The Makefile only downloads the meson subprojects when it initializes the submodule
# itself; the checkout already did that, so download them here with QEMU's own meson.
if [ ! -d qemu/subprojects/keycodemapdb ]; then
  python3 -m venv /tmp/meson
  /tmp/meson/bin/pip install -q qemu/python/wheels/meson-*.whl
  (cd qemu && /tmp/meson/bin/meson subprojects download)
fi

$APT_UPDATE
mk-build-deps --install --remove --tool "apt-get -y --no-install-recommends" debian/control
DEB_BUILD_OPTIONS="parallel=$(nproc)" make deb
ls -la ./*.deb
