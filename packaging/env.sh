# shellcheck shell=bash
# Shared setup for the packaging scripts. Source it: `. "$(dirname "$0")/../env.sh"`.
#
# Exports:
#   ROOT                  workspace root
#   VERSION               [workspace.package] version (override: NIEUWSER_VERSION)
#   DIST                  release artifact directory (default: $ROOT/dist/release)
#   APP_ID                reverse-DNS application id
#   APP_TITLE             human-readable application name
#   APP_REPO              GitHub repository (owner/name)
#   NIEUWSER_BUILD_DATE   UTC build date, YYYY-MM-DD
#   SOURCE_DATE_EPOCH     commit timestamp for reproducible package metadata
#   CARGO_TARGET_DIR      cargo target directory (default: $ROOT/target)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ROOT

workspace_version() {
  awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]") ; next }
    in_pkg && $1 == "version" { gsub(/[" ]/, "", $3); print $3; exit }
  ' "$ROOT/Cargo.toml"
}

VERSION="${NIEUWSER_VERSION:-$(workspace_version)}"
if [ -z "$VERSION" ]; then
  echo "error: could not read [workspace.package] version from $ROOT/Cargo.toml" >&2
  exit 1
fi
export VERSION

DIST="${DIST:-$ROOT/dist/release}"
mkdir -p "$DIST"
export DIST

APP_ID="io.github.frenzie.nieuwser"
APP_TITLE="Nieuwser"
APP_REPO="${GITHUB_REPOSITORY:-Frenzie/nieuwser}"
export APP_ID APP_TITLE APP_REPO

export NIEUWSER_BUILD_DATE="${NIEUWSER_BUILD_DATE:-$(date -u +%Y-%m-%d)}"
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" show -s --format=%ct HEAD 2>/dev/null || date +%s)}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

copy_docs() {
  local dest="$1" f
  for f in README.md LICENSE LICENSE-MIT LICENSE-APACHE COPYRIGHT; do
    if [ -f "$ROOT/$f" ]; then cp "$ROOT/$f" "$dest/"; fi
  done
}

sha256_of() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
