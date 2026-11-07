---
title: Claude login keeper example plugin
type: reference
status: draft
tags: [janus, plugin, example, oauth, anthropic]
---

# Claude login keeper example plugin

`claude-login-keeper` keeps a Claude subscription credential in the Janus pool
logged in. It serves the `JanusPlugin` gRPC service on a unix socket and does
four things:

1. **Notices.** It reads `credential_invalid` and `account_error` events for the
   credentials it manages, and it checks them through the admin API at startup
   and every `validate_interval`. An expiring credential is refreshed during the
   check, so a refused refresh token is found too. Checks continue during a
   flow: an automatic flow ends when its credential is usable again.
2. **Logs in.** It runs `claude auth login --claudeai` with its own `HOME` and
   `CLAUDE_CONFIG_DIR`, so the login is an OAuth grant separate from the
   operator's own Claude Code. Flows are debounced, run one at a time, and back
   off after a failure. An operator's cancel names its credential and holds
   automatic flows for that credential for `backoff_max`.
3. **Asks for the click.** It writes the authorize URL as a JSON log line
   (`event` `login_url`), shows it on a loopback page, prints it from
   `claude-login-keeper login`, and puts it in the deny reason that callers get.
4. **Rotates.** After the operator authorizes, it pushes the new `claudeAiOauth`
   material into the pool with `PUT /v1/credentials/{ref}/material`, which
   clears `invalid`. Then it deletes its own copy. A login whose rotation failed
   is kept per credential and rotated by the next attempt.

The design, including what was verified about where the CLI stores a login, is
in <ref target="docs/claude-login-keeper">the Claude login keeper
reference</ref>. The general guide is
<ref target="docs/writing-a-plugin">writing a plugin</ref>. A recorded run
against a real daemon is in
<ref target="examples/claude-login-keeper/VERIFICATION">the verification
transcript</ref>.

## Files {#files}

| File | Contents |
|---|---|
| `src/config.rs` | The TOML file. |
| `src/output.rs` | Strips ANSI escape sequences; finds the authorize URL and the CLI's failure lines. |
| `src/login.rs` | One isolated `claude auth login` run as a state machine: the shims, the environment, the pasted code, cancel and timeout. |
| `src/schedule.rs` | Debounce and exponential backoff. Has no I/O. |
| `src/admin.rs` | Show, refresh and rotate through the admin API, on the unix socket or `http://` with a token. |
| `src/keeper.rs` | Health per credential, one flow at a time, the periodic check, the rotation. |
| `src/service.rs` | `ResolveRoute` and `Notify`. |
| `src/http.rs` | The loopback page. |
| `src/lib.rs` | `serve`: `janus_core::peer::serve_unix`, which binds the socket with the modes that the daemon requires. |
| `src/main.rs` | `serve` and `login`. |
| `tests/flow.rs` | End-to-end tests over the gRPC socket, the page and the `login` binary, with a fake admin API (several credentials, injected failures) and `tests/fake-claude.sh`. |
| `tests/fake-claude.sh` | A stand-in for `claude auth login`. |
| `keeper.example.toml` | An annotated configuration file. |

The crate is a member of the workspace and uses only workspace dependencies. The
crate is unix-only.

## Build and test {#build}

From the repository root (`protoc` must be on `PATH`):

```sh
cargo build -p claude-login-keeper        # target/debug/claude-login-keeper
cargo test  -p claude-login-keeper        # unit tests + tests/flow.rs
```

The workspace gates (`fmt`, `clippy -D warnings`, `test`, MSRV 1.89.0 with
`--locked`, `cargo deny`) cover the crate.

## Run {#run}

1. Put the credential in the pool. It must be a pool slot, because a
   config-sourced credential cannot be rotated:

   ```sh
   janus credential add --upstream anthropic --ref claude-sub --from-file creds.json
   ```

2. Copy `keeper.example.toml` and set `socket`, `state_dir`, `claude_bin` and
   `[[credential]]`. List an instance in `instances` only if this credential is
   its only source (<ref target="docs/claude-login-keeper#deny">deny or
   default</ref>).
3. Point the daemon at the socket, in its config:

   ```toml
   [plugin]
   endpoint   = "unix:///home/op/.janus-keeper/run/keeper.sock"
   timeout_ms = 250
   ```

4. Start the keeper, then check the channel:

   ```sh
   claude-login-keeper serve --config keeper.toml 2>>keeper.log &
   janus plugin check
   ```

When a login is needed, do one of these:

- Open `http://127.0.0.1:27711/`, open the authorize link, sign in, and paste
  the code the page shows.
- Run `claude-login-keeper login --config keeper.toml --credential claude-sub`,
  open the URL that it prints, and paste the code on its stdin. The command uses
  the running keeper. It follows only the flow whose `id` its start returned. If
  that keeper is busy with an attempt for the same credential, the command
  follows that attempt's flow; if the attempt is busy with another credential,
  the command exits 2. When no keeper answers, it runs one in its own process.
  Ctrl-C cancels the flow for `--credential` only, and with an in-process keeper
  the command waits up to 10 s for the child to end. Exit codes:

  | Code | Meaning |
  |---|---|
  | 0 | The login was rotated into the pool. |
  | 1 | The login failed or was cancelled. |
  | 2 | An error: the config file, an unknown `--credential`, or the keeper. The message is one line on stderr. |

On the keeper's host, the second URL (the local URL) completes without a pasted
code.

## Limits {#limits}

See <ref target="docs/claude-login-keeper#limits">the reference's limits</ref>.
In short:

- The page has no authentication and binds only to loopback.
- The CLI's output strings are matched as Claude Code 2.1.319 prints them.
- State is in memory.
- A complete real login has not run in tests.
