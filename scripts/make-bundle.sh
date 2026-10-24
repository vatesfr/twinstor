#!/usr/bin/env bash
# Assemble the TwinStor bundle: a self-contained tarball with the static
# binary, the systemd units, the runtime dependency RPMs and the
# install/uninstall scripts.
#
# The RPMs are not kept in git. Missing ones are downloaded from the URLs in
# deps/SOURCES into deps/ (or $TWINSTOR_DEPS), and each must match
# deps/SHA256SUMS. See deps/README.md.
#
# Usage:
#   scripts/make-bundle.sh                                  # build the musl binary
#   TWINSTOR_BIN=/path/to/twinstor scripts/make-bundle.sh   # use a prebuilt one
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
TARGET="x86_64-unknown-linux-musl"
BIN="${TWINSTOR_BIN:-target/$TARGET/release/twinstor}"
DEPS="${TWINSTOR_DEPS:-deps}"

# Fetch any missing dependency RPM from deps/SOURCES, then check them all
# before the build so a bad one fails early.
mkdir -p "$DEPS"
missing=0
while read -r sum file; do
  if [ ! -f "$DEPS/$file" ]; then
    url="$(grep -m1 "/$file\$" deps/SOURCES || true)"
    if [ -n "$url" ]; then
      echo "==> fetching $file"
      curl -fsSL --retry 3 -o "$DEPS/$file.part" "$url" && mv "$DEPS/$file.part" "$DEPS/$file"
    fi
  fi
  if [ ! -f "$DEPS/$file" ]; then
    echo "ERROR: missing dependency $DEPS/$file" >&2
    missing=1
  elif [ "$(sha256sum "$DEPS/$file" | cut -d' ' -f1)" != "$sum" ]; then
    echo "ERROR: checksum mismatch for $DEPS/$file" >&2
    missing=1
  fi
done < deps/SHA256SUMS
if [ "$missing" -ne 0 ]; then
  echo "       See deps/README.md." >&2
  exit 1
fi

if [ -n "${TWINSTOR_BIN:-}" ]; then
  echo "==> using prebuilt binary: $BIN"
else
  # Always run cargo: it is a no-op when nothing changed, and a leftover
  # binary from an older version must never end up in the bundle.
  echo "==> building static binary ($TARGET)"
  rustup target add "$TARGET" >/dev/null 2>&1 || true
  cargo build --release --target "$TARGET"
fi

# Refuse to assemble a mislabeled bundle: the binary must report the version
# the tarball is named for.
BIN_VER="$("$BIN" --version 2>/dev/null | awk '{print $NF}')"
if [ "$BIN_VER" != "$VERSION" ]; then
  echo "ERROR: binary version '$BIN_VER' != Cargo.toml version '$VERSION'." >&2
  echo "       Refusing to build twinstor-test-$VERSION.tar.gz around the wrong binary." >&2
  echo "       Rebuild ($BIN) or unset TWINSTOR_BIN." >&2
  exit 1
fi

NAME="twinstor-test-$VERSION"
ROOT="$(mktemp -d)"
STAGE="$ROOT/$NAME"
mkdir -p "$STAGE/deps" "$STAGE/systemd"

install -m 0755 "$BIN"                        "$STAGE/twinstor"
install -m 0644 config/twinstor.service       "$STAGE/systemd/twinstor.service"
install -m 0644 config/twinstor-reboot-drain.service "$STAGE/systemd/twinstor-reboot-drain.service"
install -m 0644 config/twinstor.toml.example  "$STAGE/twinstor.toml.example"
install -m 0755 scripts/install.sh            "$STAGE/install.sh"
install -m 0755 scripts/uninstall.sh          "$STAGE/uninstall.sh"
install -m 0644 README.md                     "$STAGE/README.md"
install -m 0644 LICENSE                       "$STAGE/LICENSE"
while read -r _ file; do
  install -m 0644 "$DEPS/$file" "$STAGE/deps/$file"
done < deps/SHA256SUMS

OUT="$REPO/$NAME.tar.gz"
tar -C "$ROOT" -czf "$OUT" "$NAME"
rm -rf "$ROOT"

echo
echo "bundle: $OUT"
echo "size:   $(du -h "$OUT" | cut -f1)"
echo
echo "On each host:"
echo "    curl -L <link> -o $NAME.tar.gz"
echo "    tar xzf $NAME.tar.gz && cd $NAME && sudo ./install.sh"
