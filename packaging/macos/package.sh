#!/usr/bin/env bash
# Build and package Nieuwser for macOS:
#
#   $DIST/nieuwser-<version>-macos-universal.dmg       Nieuwser.app (ad-hoc signed, not notarized)
#   $DIST/nieuwser-cli-<version>-macos-universal.zip   headless CLI
#
# Usage: packaging/macos/package.sh [--skip-build]
#
# This intentionally uses no signing identity or Apple credentials. The app is ad-hoc signed so
# macOS can execute it; Gatekeeper will still warn when it is downloaded from the internet.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/macos"

SKIP_BUILD=0
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    -h | --help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
BASENAME="nieuwser-$VERSION-macos-universal"
WORK="$CARGO_TARGET_DIR/macos-package"
APP="$WORK/Nieuwser.app"
rm -rf "$WORK"

if [ "$SKIP_BUILD" = 0 ]; then
  (cd "$ROOT" && cargo build --release --locked -p nieuwser -p nieuwser-cli --target aarch64-apple-darwin)
  (cd "$ROOT" && cargo build --release --locked -p nieuwser -p nieuwser-cli --target x86_64-apple-darwin)
fi

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create \
  "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/nieuwser" \
  "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/nieuwser" \
  -output "$APP/Contents/MacOS/nieuwser"
lipo -create \
  "$CARGO_TARGET_DIR/aarch64-apple-darwin/release/nieuwser-cli" \
  "$CARGO_TARGET_DIR/x86_64-apple-darwin/release/nieuwser-cli" \
  -output "$WORK/nieuwser-cli"
chmod 755 "$APP/Contents/MacOS/nieuwser" "$WORK/nieuwser-cli"

sed -e "s/@APP_ID@/$APP_ID/g" \
  -e "s/@APP_TITLE@/$APP_TITLE/g" \
  -e "s/@VERSION@/$VERSION/g" \
  -e "s/@BUNDLE_VERSION@/${VERSION%%-*}/g" \
  "$HERE/Info.plist.in" >"$APP/Contents/Info.plist"

# Build the .icns from the committed 1024 px PNG using the macOS tools.
ICONSET="$WORK/nieuwser.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ROOT/assets/app-icon/nieuwser-1024.png" \
    --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z "$((size * 2))" "$((size * 2))" "$ROOT/assets/app-icon/nieuwser-1024.png" \
    --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/nieuwser.icns"

# No Developer ID: ad-hoc signing is enough for local execution on Apple Silicon.
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP"

# DMG with the usual drag-to-Applications layout.
DMG="$WORK/dmg"
mkdir -p "$DMG"
cp -R "$APP" "$DMG/Nieuwser.app"
ln -s /Applications "$DMG/Applications"
hdiutil create -volname "$APP_TITLE" -srcfolder "$DMG" -ov -format UDZO \
  "$DIST/$BASENAME.dmg" >/dev/null
echo "wrote $DIST/$BASENAME.dmg"

# CLI zip with a versioned top-level directory.
CLI="$WORK/nieuwser-cli-$VERSION-macos-universal"
mkdir -p "$CLI"
cp "$WORK/nieuwser-cli" "$CLI/"
copy_docs "$CLI"
(cd "$WORK" && ditto -c -k --norsrc --keepParent "$CLI" "$DIST/nieuwser-cli-$VERSION-macos-universal.zip")
echo "wrote $DIST/nieuwser-cli-$VERSION-macos-universal.zip"

echo "==> release assets:"
ls -lh "$DIST"/*"$VERSION"*
