#!/usr/bin/env bash
# Build and package Nieuwser for Linux (<arch> is x86_64 or aarch64):
#
#   $DIST/nieuwser-<version>-linux-<arch>.AppImage
#   $DIST/nieuwser-<version>-linux-<arch>.deb
#   $DIST/nieuwser-<version>-linux-<arch>.tar.gz
#
# Usage: packaging/linux/package.sh [--skip-build]
#
# Build on an old distro (CI: Ubuntu 22.04) so the AppImage and binaries run on newer ones.
# The AppImage tooling is downloaded at verified pins into $CARGO_TARGET_DIR.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/linux"

SKIP_BUILD=0
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    -h | --help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ARCH="${NIEUWSER_ARCH:-$(uname -m)}"
case "$ARCH" in
  x86_64) DEB_ARCH=amd64 ;;
  aarch64 | arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
  *) echo "unsupported architecture $ARCH" >&2; exit 2 ;;
esac
BASENAME="nieuwser-$VERSION-linux-$ARCH"
MAINTAINER="${NIEUWSER_MAINTAINER:-Frenzie <frenzie@users.noreply.github.com>}"

echo "==> Nieuwser $VERSION for Linux $ARCH"

if [ "$SKIP_BUILD" = 0 ]; then
  (cd "$ROOT" && cargo build --release --locked -p nieuwser -p nieuwser-cli)
fi
BIN="$CARGO_TARGET_DIR/release"
WORK="$CARGO_TARGET_DIR/linux-package"
STAGE="$WORK/root"
rm -rf "$WORK"

# ---- stage an FHS tree (shared by every format) ------------------------------------------------
install -Dm755 "$BIN/nieuwser" "$STAGE/usr/bin/nieuwser"
install -Dm755 "$BIN/nieuwser-cli" "$STAGE/usr/bin/nieuwser-cli"
install -Dm644 "$HERE/$APP_ID.desktop" "$STAGE/usr/share/applications/$APP_ID.desktop"
mkdir -p "$STAGE/usr/share/metainfo"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@DATE@/$NIEUWSER_BUILD_DATE/g" \
  "$HERE/$APP_ID.metainfo.xml.in" >"$STAGE/usr/share/metainfo/$APP_ID.metainfo.xml"
mkdir -p "$STAGE/usr/share/icons"
cp -R "$ROOT/assets/app-icon/hicolor" "$STAGE/usr/share/icons/"
mkdir -p "$STAGE/usr/share/doc/nieuwser"
copy_docs "$STAGE/usr/share/doc/nieuwser"
# Normalize modes: 755 for binaries/directories, 644 for data files.
chmod -R u+rwX,go+rX,go-w "$STAGE"

if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$STAGE/usr/share/applications/$APP_ID.desktop"
fi
if command -v appstreamcli >/dev/null; then
  appstreamcli validate --no-net --explain "$STAGE/usr/share/metainfo/$APP_ID.metainfo.xml"
fi

# Smoke-test the headless CLI; the GUI needs a display.
"$STAGE/usr/bin/nieuwser-cli" --help >/dev/null

# ---- .tar.gz -----------------------------------------------------------------------------------
mkdir -p "$WORK/tar"
cp -R "$STAGE/usr" "$WORK/tar/$BASENAME"
tar -C "$WORK/tar" -czf "$DIST/$BASENAME.tar.gz" "$BASENAME"
echo "wrote $DIST/$BASENAME.tar.gz"

# ---- .deb --------------------------------------------------------------------------------------
DEB="$WORK/deb"
mkdir -p "$DEB/DEBIAN"
cp -R "$STAGE/usr" "$DEB/"
INSTALLED_SIZE="$(du -sk "$DEB/usr" | cut -f1)"
cat >"$DEB/DEBIAN/control" <<EOF
Package: nieuwser
Version: $VERSION
Section: net
Priority: optional
Architecture: $DEB_ARCH
Installed-Size: $INSTALLED_SIZE
Maintainer: $MAINTAINER
Homepage: https://github.com/$APP_REPO
Depends: libc6, libgcc-s1, libfontconfig1, libfreetype6
Description: Native offline-first desktop client for FreshRSS
 Nieuwser is a native reader for FreshRSS. It keeps a local SQLite cache of
 feeds and articles, supports local search, and queues read and starred
 changes until the next sync.
EOF
dpkg-deb --build --root-owner-group "$DEB" "$DIST/$BASENAME.deb" >/dev/null
echo "wrote $DIST/$BASENAME.deb"

# ---- AppImage ----------------------------------------------------------------------------------
download_verified() {
  local url="$1" out="$2" sha="$3"
  if [ -f "$out" ] && [ "$(sha256_of "$out")" = "$sha" ]; then return 0; fi
  mkdir -p "$(dirname "$out")"
  curl -fL --retry 3 --retry-delay 2 -o "$out.tmp" "$url"
  local got
  got="$(sha256_of "$out.tmp")"
  if [ "$got" != "$sha" ]; then
    rm -f "$out.tmp"
    echo "error: checksum mismatch for $url (got $got, want $sha)" >&2
    exit 1
  fi
  mv "$out.tmp" "$out"
}

# Digests from the official GitHub release asset metadata:
# https://github.com/AppImage/appimagetool/releases/tag/1.9.1
# https://github.com/AppImage/type2-runtime/releases/tag/20251108
TOOL_VERSION=1.9.1
RUNTIME_VERSION=20251108
case "$ARCH" in
  x86_64)
    TOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    ;;
  aarch64)
    TOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
    ;;
esac
TOOL="$CARGO_TARGET_DIR/appimagetool-$TOOL_VERSION-$ARCH.AppImage"
RUNTIME="$CARGO_TARGET_DIR/type2-runtime-$RUNTIME_VERSION-$ARCH"
download_verified \
  "https://github.com/AppImage/appimagetool/releases/download/$TOOL_VERSION/appimagetool-$ARCH.AppImage" \
  "$TOOL" "$TOOL_SHA256"
download_verified \
  "https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_VERSION/runtime-$ARCH" \
  "$RUNTIME" "$RUNTIME_SHA256"
chmod 755 "$TOOL"

APPDIR="$WORK/Nieuwser.AppDir"
cp -R "$STAGE" "$APPDIR"
cat >"$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/nieuwser" "$@"
EOF
chmod 755 "$APPDIR/AppRun"
cp "$HERE/$APP_ID.desktop" "$APPDIR/$APP_ID.desktop"
cp "$ROOT/assets/app-icon/hicolor/256x256/apps/$APP_ID.png" "$APPDIR/$APP_ID.png"
chmod 644 "$APPDIR/$APP_ID.desktop" "$APPDIR/$APP_ID.png"
ln -s "$APP_ID.png" "$APPDIR/.DirIcon"

OUT="$(cd "$DIST" && pwd)/$BASENAME.AppImage"
APPDIR="$(cd "$APPDIR" && pwd)"
TOOL="$(cd "$(dirname "$TOOL")" && pwd)/$(basename "$TOOL")"
RUNTIME="$(cd "$(dirname "$RUNTIME")" && pwd)/$(basename "$RUNTIME")"
ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 \
  "$TOOL" --no-appstream --runtime-file "$RUNTIME" "$APPDIR" "$OUT"
echo "wrote $OUT"

echo "==> release assets:"
ls -lh "$DIST"/*"$VERSION"*
