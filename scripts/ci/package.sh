#!/usr/bin/env bash
# Package a release tarball from already-built binaries.
#   scripts/ci/package.sh <version> <target-triple> <target-dir> <out-dir>
# <target-dir>/release must hold janus-daemon and janus; ui/dist must be built.
# Layout (the daemon resolves UI assets at <exe_dir>/ui):
#   janus-<version>-<target>/{janus-daemon,janus,ui/,LICENSE-*,README.md}
set -euo pipefail
cd "$(dirname "$0")/../.."
version="$1"; target="$2"; bin_dir="$3"; out="$4"
name="janus-${version}-${target}"
stage="$(mktemp -d)/${name}"
mkdir -p "$stage" "$out"
cp "$bin_dir/janus-daemon" "$bin_dir/janus" "$stage/"
cp -R ui/dist "$stage/ui"
cp LICENSE-MIT LICENSE-APACHE README.md "$stage/"
tar -C "$(dirname "$stage")" -czf "$out/${name}.tar.gz" "$name"
echo "$out/${name}.tar.gz"
