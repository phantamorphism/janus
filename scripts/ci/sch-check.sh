#!/usr/bin/env bash
# Run `sch check` for a PR and render a report.
#
# Inputs (env):
#   TYPESAFE_API_KEY  enables the propagation judge (jev). Unset or empty
#                     (forks, no secrets) => deterministic rules only.
#   SCH_BASE          base ref to compare against (default origin/main)
#   OUT_DIR           where to write report.md and check.json (default ${RUNNER_TEMP:-/tmp}/sch)
#
# Exit: 0 clean or warnings only; 1 rule errors; 3 judge budget exceeded;
# 2/8 usage or malformed corpus. A judge outage (exit 4) degrades to the
# deterministic rules with a loud notice rather than failing the PR.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1
base="${SCH_BASE:-origin/main}"
out="${OUT_DIR:-${RUNNER_TEMP:-/tmp}/sch}"
mkdir -p "$out"
json="$out/check.json"; report="$out/report.md"

# sch compares main..HEAD and needs a local `main` branch.
if [ "$(git rev-parse --abbrev-ref HEAD)" != "main" ]; then
  git branch -f main "$base" >/dev/null
fi

mode="full (propagation judge jev)"
restore=""
deterministic() {
  # Turn the judge-backed rule off for this run only; restored on exit.
  cp schematic.yaml "$out/schematic.yaml.orig"
  restore="$out/schematic.yaml.orig"
  python3 - <<'PY'
import re
s = open("schematic.yaml").read()
if re.search(r"^rules:\s*\{\}\s*$", s, re.M):
    s = re.sub(r"^rules:\s*\{\}\s*$", "rules:\n  propagation/may-need-update: off", s, flags=re.M)
elif re.search(r"^rules:\s*$", s, re.M):
    s = re.sub(r"^rules:\s*$", "rules:\n  propagation/may-need-update: off", s, count=1, flags=re.M)
else:
    s += "\nrules:\n  propagation/may-need-update: off\n"
open("schematic.yaml", "w").write(s)
PY
}
trap '[ -n "$restore" ] && cp "$restore" schematic.yaml' EXIT

notice=""
if [ -z "${TYPESAFE_API_KEY:-}" ]; then
  mode="deterministic rules only (no TYPESAFE_API_KEY: fork PR or secret not configured)"
  notice="Propagation warnings were NOT evaluated on this run (no judge credentials). Maintainers get the full check after merge into a branch in this repo."
  deterministic
fi

sch check --json > "$json"; code=$?
if [ "$code" = 4 ] && [ -z "$restore" ]; then
  mode="deterministic rules only (propagation judge unreachable)"
  notice="The propagation judge failed; propagation warnings were NOT evaluated on this run. Re-run the job to retry."
  deterministic
  sch check --json > "$json"; code=$?
fi

python3 - "$json" "$mode" "$notice" "$code" > "$report" <<'PY'
import json, sys
path, mode, notice, code = sys.argv[1:5]
try:
    d = json.load(open(path))
except Exception as e:
    print(f"## Schematic check\n\n`sch check` produced no parseable output (exit {code}): {e}")
    sys.exit(0)
print("## Schematic check\n")
print(f"Mode: **{mode}**\n")
if notice:
    print(f"> {notice}\n")
if "error" in d:
    print(f"**sch failed (exit {d.get('exit', code)}):** {d['error'].get('message')}")
    sys.exit(0)
s = d.get("summary", {})
print(f"{s.get('errors', 0)} error(s), {s.get('warnings', 0)} warning(s).\n")
diags = d.get("diagnostics", [])
for sev, title in (("error", "Errors (block the PR)"), ("warn", "Propagation and style warnings (do not block)")):
    rows = [x for x in diags if x.get("severity") == sev]
    if not rows:
        continue
    print(f"### {title}\n")
    for x in rows:
        loc = x.get("document", "?") + (f":{x['line']}" if x.get("line") else "")
        print(f"- `{x.get('rule')}` {loc}: {x.get('message')}")
    print()
PY
cat "$report"
[ -n "${GITHUB_STEP_SUMMARY:-}" ] && cat "$report" >> "$GITHUB_STEP_SUMMARY"

# Annotations for the PR "Files" view.
python3 - "$json" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
for x in d.get("diagnostics", []):
    lvl = "error" if x.get("severity") == "error" else "warning"
    msg = (x.get("message") or "").replace("\n", " ")
    line = x.get("line") or 1
    print(f"::{lvl} file={x.get('document')},line={line},title={x.get('rule')}::{msg}")
PY
exit "$code"
