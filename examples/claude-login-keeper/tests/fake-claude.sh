#!/bin/sh
# A stand-in for `claude auth login --claudeai`, for the keeper's tests and
# VERIFICATION.md. It prints what the real CLI prints, asks the `open` on
# PATH to open the localhost-callback URL, prompts for a pasted code, and
# writes $CLAUDE_CONFIG_DIR/.credentials.json for a code starting `good`.
#
# Control and records live in $CLAUDE_CONFIG_DIR/../fake-claude/ (the
# keeper clears the environment, so nothing is passed by variable):
#   mode         "normal" (default) or "hang" (never prompts, never exits)
#   runs         one line per invocation
#   env.<pid>    the environment it ran with
#   security.path  the `security` that PATH resolves to
#   security.rc    the exit status of `security find-generic-password` on
#                  the unsuffixed item name, as the real CLI's lookup of a
#                  default config would be; "not run" when PATH resolves to
#                  the system binary, which is then never called
ctl="$CLAUDE_CONFIG_DIR/../fake-claude"
mkdir -p "$ctl"
echo "run $$" >> "$ctl/runs"
env > "$ctl/env.$$"
sec=$(command -v security)
shim="$(cd "$CLAUDE_CONFIG_DIR/.." && pwd)/bin/security"
echo "$sec" > "$ctl/security.path"
if [ "$sec" = "$shim" ]; then
    "$sec" find-generic-password -s "Claude Code-credentials" > /dev/null 2>&1
    echo $? > "$ctl/security.rc"
else
    echo "not run" > "$ctl/security.rc"
fi
if [ "$1 $2 $3" != "auth login --claudeai" ]; then
    echo "unexpected arguments: $*" >&2
    exit 2
fi
mode=$(cat "$ctl/mode" 2> /dev/null || echo normal)
state="FAKESTATE$$"
printf 'Opening browser to sign in\342\200\246\n'
open "https://claude.com/cai/oauth/authorize?code=true&client_id=fake&redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback&state=$state"
url="https://claude.com/cai/oauth/authorize?code=true&client_id=fake&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&state=$state"
printf "If the browser didn't open, visit: \033]8;;%s\007%s\033]8;;\007\n" "$url" "$url"
if [ "$mode" = hang ]; then
    while :; do sleep 1; done
fi
while :; do
    printf 'Paste code here if prompted > '
    IFS= read -r code || exit 1
    case "$code" in
        good*)
            expires="$(($(date +%s) + 28800))000"
            cat > "$CLAUDE_CONFIG_DIR/.credentials.json" << EOF
{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKE-NEW-$$","refreshToken":"sk-ant-ort01-FAKE-NEW-$$","expiresAt":$expires,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}
EOF
            echo "Login successful."
            exit 0
            ;;
        fail*)
            echo "Login failed: Request failed with status code 400"
            exit 1
            ;;
        *)
            echo "Invalid code. Please make sure the full code was copied."
            ;;
    esac
done
