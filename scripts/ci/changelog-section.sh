#!/usr/bin/env bash
# Print the CHANGELOG.md section for a version ("v1.2.3" or "1.2.3"), headed
# "## [1.2.3] - date" or "## 1.2.3". Exits 1 with no output when the file or
# the section is missing or empty; the caller falls back to generated notes.
set -euo pipefail
cd "$(dirname "$0")/../.."
v="${1#v}"
[ -f CHANGELOG.md ] || exit 1
awk -v v="$v" '
  /^## / {
    if (on) exit
    h = $0; sub(/^## +\[?/, "", h)
    if (index(h, v) == 1 && substr(h, length(v) + 1, 1) !~ /[0-9.]/) on = 1
    next
  }
  on {
    if ($0 ~ /^[[:space:]]*$/) { blank++; next }
    if (n++ > 0) for (; blank > 0; blank--) print ""
    blank = 0; print
  }
  END { exit (n > 0 ? 0 : 1) }
' CHANGELOG.md
