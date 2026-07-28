#!/bin/bash
# Usage: stamp-version.sh <version>
# Rewrite the package version in Cargo.toml. Used by the nightly workflow, where
# every versioned artifact — the binary's CARGO_PKG_VERSION, asset names, the DMG
# plist, the Inno installer — reads Cargo.toml, so this one edit covers them all.
# Cargo.lock is left alone: cargo refreshes the root package's own lock entry
# automatically, without network access.
#
# Extracted from the inline step so nightly's several build jobs stamp
# identically, and so the guard below has exactly one home.
set -euo pipefail

VERSION="$1"

awk -v ver="$VERSION" '!done && /^version = /{ $0 = "version = \"" ver "\""; done = 1 } { print }' \
  Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml

# Fail loudly if the substitution missed. Without this the awk is a silent no-op
# whenever Cargo.toml's shape changes — e.g. if the root manifest ever becomes a
# virtual workspace whose members carry `version.workspace = true`, in which case
# the version to stamp moves to `[workspace.package]`. A nightly that quietly
# publishes assets named after the *last stable* version is far worse than one
# that fails here.
if ! grep -qx "version = \"$VERSION\"" Cargo.toml; then
  echo "::error::stamp-version.sh did not stamp $VERSION into Cargo.toml — the manifest's version line is not where this script expects it"
  exit 1
fi

# Report the line actually stamped, not the first thing that looks like a
# version: post-crate-split the root package carries `version.workspace = true`
# and the real value lives further down under [workspace.package], so a naive
# `grep -m1 '^version'` prints the inherit marker and tells you nothing.
grep -n "^version = \"$VERSION\"" Cargo.toml
