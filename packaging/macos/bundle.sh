#!/bin/sh
# Make "Multi-pass Scanner.app" from the release binary.  Run on a Mac, from
# the folder that holds Cargo.toml, after `cargo build --release`:
#
#     packaging/macos/bundle.sh            ->  target/release/Multi-pass Scanner.app
#
# The app is signed ad hoc, which is enough to run it on the Mac that built it.
# It links the OpenCV libraries where the build found them (Homebrew), so it
# only runs on a Mac that has those installed.  Giving the app to other people
# needs OpenCV built into it, a Developer ID signature and notarisation.
set -eu

BIN=target/release/multipass-scanner
APP="target/release/Multi-pass Scanner.app"
HERE=packaging/macos

[ "$(uname -s)" = Darwin ] || { echo "bundle.sh: this script is for macOS" >&2; exit 1; }
[ -x "$BIN" ] || { echo "bundle.sh: run 'cargo build --release' first" >&2; exit 1; }

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/multipass-scanner"
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"

# The icon: every size macOS wants, from the 1024 px PNG (itself rendered from
# packaging/multipass-scanner.svg).
ICONSET=$(mktemp -d)/AppIcon.iconset
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    sips -z $size $size "$HERE/icon-1024.png" --out "$ICONSET/icon_${size}x${size}.png" > /dev/null
    double=$((size * 2))
    sips -z $double $double "$HERE/icon-1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"

codesign --force --sign - "$APP"
echo "made $APP"
echo "to install: cp -R \"$APP\" /Applications/"
