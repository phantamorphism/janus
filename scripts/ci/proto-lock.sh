#!/usr/bin/env bash
# Proto drift check. proto/janus.proto is the plugin contract's wire definition;
# any change to it must be deliberate and land together with the docs and the
# plugin-contract version it implies. The expected digest lives in
# scripts/ci/proto.sha256.
#   scripts/ci/proto-lock.sh            verify (CI)
#   scripts/ci/proto-lock.sh --update   re-pin after an intentional change
set -euo pipefail
cd "$(dirname "$0")/../.."
lock=scripts/ci/proto.sha256
if [ "${1:-}" = "--update" ]; then
  node -e 'const c=require("crypto"),f=require("fs");process.stdout.write(c.createHash("sha256").update(f.readFileSync("proto/janus.proto")).digest("hex")+"\n")' > "$lock"
  echo "pinned $(cat "$lock")"
  exit 0
fi
if ! node scripts/gates/file-hash.mjs proto/janus.proto "$(tr -d '[:space:]' < "$lock")"; then
  echo "::error file=proto/janus.proto::proto/janus.proto changed. If intentional, update the plugin-contract docs, then run scripts/ci/proto-lock.sh --update and commit scripts/ci/proto.sha256." >&2
  exit 1
fi
