#!/usr/bin/env bash
# Build the Schematic CLI (`sch`) from source at a pinned commit and put it in
# <dest> (default: $HOME/.local/bin). Requires Bun >= 1.4 (setup-bun in CI).
# SCH_REPO_TOKEN, when set, is used to clone (needed while the repo is private).
set -euo pipefail
SCH_REPO="${SCH_REPO:-phantamorphism/schematic}"
SCH_REF="${SCH_REF:-0b2f11bc219b8ce0c64b1e431536f3e7c598d58d}"   # pin; bump deliberately
dest="${1:-$HOME/.local/bin}"
work="$(mktemp -d)"
auth=""
[ -n "${SCH_REPO_TOKEN:-}" ] && auth="x-access-token:${SCH_REPO_TOKEN}@"
git clone --quiet "https://${auth}github.com/${SCH_REPO}.git" "$work/schematic"
git -C "$work/schematic" checkout --quiet "$SCH_REF"
(cd "$work/schematic" && bun install --frozen-lockfile && bun run build)
mkdir -p "$dest"
install -m 0755 "$work/schematic/dist/sch" "$dest/sch"
rm -rf "$work"
echo "installed sch ($SCH_REF) to $dest/sch"
