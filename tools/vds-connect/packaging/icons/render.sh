#!/bin/sh
# Renders vds-connect.svg into out/: PNGs, a Windows .ico and a macOS .icns.
# Needs rsvg-convert (librsvg2-bin), icotool (icoutils) and png2icns (icnsutils).
set -eu

cd "$(dirname "$0")"
rm -rf out
mkdir out

for size in 16 32 48 64 128 256 512 1024; do
	rsvg-convert -w "$size" -h "$size" -o "out/vds-connect-$size.png" vds-connect.svg
done
cp out/vds-connect-256.png out/vds-connect.png

icotool -c -o out/vds-connect.ico \
	out/vds-connect-16.png out/vds-connect-32.png out/vds-connect-48.png \
	out/vds-connect-64.png out/vds-connect-128.png out/vds-connect-256.png
png2icns out/vds-connect.icns \
	out/vds-connect-16.png out/vds-connect-32.png out/vds-connect-128.png \
	out/vds-connect-256.png out/vds-connect-512.png out/vds-connect-1024.png
