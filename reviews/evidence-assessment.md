---
title: evidence-assessment
type: reference
status: draft
---

# Evidence assessment: Janus D1-D110 (re-assessment)

VERDICT: PASS

REVIEWER: independent-evidence-assessor (claude-sonnet-5-5)

Method: read DELIVERABLES.md (including the diff of changes), results.json (110
PASS, fresh run), every acceptance script (a, b, cd, ef, ghn + ghn-h, ijk, lm
line by line), and sampled artifacts (a/D4, cd/D35/D43, ghn/D60/D71, lm
D104-D109 checks/network logs/secret scan). No group script was re-run by me.
ijk-lib.mjs, lm-env.mjs, lm-worker.mjs and ghn-h/tests/conf.rs were only skimmed
for harness plumbing, not line-read; no assertion logic lives there.

## Per-deliverable table

| id | sufficient | reason |
|---|---|---|
| D1 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D2 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D3 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D4 | yes | reworded: API keys (x-api-key / Bearer) and Anthropic OAuth Bearer observed at recording upstreams, pool bytes present, caller token absent; Codex account header and live vendors accepted-excluded (see exclusions) |
| D5 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D6 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D7 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D8 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D9 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D10 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D11 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D12 | yes | error kinds for (b)-(d) asserted; spec does not pin names |
| D13 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D14 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D15 | yes | (b)/(c) accept refusal or rejection, both allowed by spec |
| D16 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D17 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D18 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D19 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D20 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D21 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D22 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D23 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D24 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D25 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D26 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D27 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D28 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D29 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D30 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D31 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D32 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D33 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D34 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D35 | yes | rejection with TLS alert -> plugin_rejected_client_cert/health rejected_client_cert; bare TCP reset asserted as plugin_unreachable (negative); spec reworded to match |
| D36 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D37 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D38 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D39 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D40 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D41 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D42 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D43 | yes | 402/billing/credit-400 emit account_error; 403 asserted NOT to emit (negative) |
| D44 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D45 | yes | notify event delivery asserted; linked half asserted in D36 linked run with call-count equality (note rewritten) |
| D46 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D47 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D48 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D49 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D50 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D51 | yes | (a)(b) real; (c) emulated via uid mismatch config; accepted-excluded (see exclusions) |
| D52 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D53 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D54 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D55 | yes | printed config adapted (server_name, 127.0.0.1) - minor |
| D56 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D57 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D58 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D59 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D60 | yes | Anthropic: full path incl. refresh-failure -> 503 not forwarded; OpenAI/Codex: refresh itself observed (token endpoint called, expires_at moves); Codex forward declared not black-box observable in spec |
| D61 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D62 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D63 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D64 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D65 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D66 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D67 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D68 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D69 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D70 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D71 | yes | four sub-cases now separate lines D71-a..d with explicit expected values |
| D72 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D73 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D74 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D75 | yes | Durable, restart, second-process lock, WAL, SIGKILL durability asserted; synchronous=FULL accepted-excluded and stated as not verified |
| D76 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D77 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D78 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D79 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D80 | yes | real assertions: baseline record, 3 requests 200 at ~1ms under external BEGIN IMMEDIATE, records NOT visible during lock, all 4 appear after release |
| D81 | yes | retention rendered '30 days' (regex accepts 30d/30 days); disabled mode and 1m purge proven |
| D82 | yes | mode 0600 + owner + socket works without token; other-user access accepted-excluded |
| D83 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D84 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D85 | yes | no/wrong/non-Bearer refused, JSON shape, non-loopback refusals, TLS and plaintext listeners; constant-time compare accepted-excluded |
| D86 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D87 | yes | spec corrected: [ui] without bind refused; loopback bind reported; assets, auth, no CORS headers, non-loopback refusal, UI disabled |
| D88 | yes | spec corrected: principals socket/admin_tcp asserted in daemon log lines naming the credential |
| D89 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D90 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D91 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D92 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D93 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D94 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D95 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D96 | yes | ?token= refused, filter, content-type, ping periodic asserted |
| D97 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D98 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D99 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D100 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D101 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D102 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D103 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |
| D104 | yes | sessionStorage only; no localStorage/cookie/URL; Authorization on every /v1 request via recording proxy; CSP headers; wrong token refused; artifact secret scan clean |
| D105 | yes | UI values compared to CLI --json output in a real browser; screenshots and checks present; assertions are real comparisons |
| D106 | yes | UI values compared to CLI --json output in a real browser; screenshots and checks present; assertions are real comparisons |
| D107 | yes | UI values compared to CLI --json output in a real browser; screenshots and checks present; assertions are real comparisons |
| D108 | yes | UI values compared to CLI --json output in a real browser; screenshots and checks present; assertions are real comparisons |
| D109 | yes | UI values compared to CLI --json output in a real browser; screenshots and checks present; assertions are real comparisons |
| D110 | yes | assertions map to the Acceptance clauses; artifacts agree with notes; controls present |

## Accepted black-box exclusions (with compensating control)

- D4, Codex account header and live vendors: Codex credentials go to a fixed
  vendor host that base_url does not redirect, so the header cannot be observed.
  Compensating control: API-key and Anthropic OAuth forms observed at recording
  upstreams; D60 shows the Codex refresh itself. Live-vendor acceptance is an
  operator check, stated in the spec.
- D51(c), other OS user: no second OS user (sudo needs a password). Compensating
  control: plugin configured to expect uid different from the daemon's, proving
  the peer-uid comparison rejects; the matching-uid case is accepted.
- D75, synchronous=FULL: per-connection setting not observable externally;
  power-loss durability untestable. Compensating control: Durable status,
  restart persistence, SIGKILL-of-writer durability, WAL journal mode,
  single-writer lock. The evidence states plainly it is not verified.
- D82, other-user access to admin.sock: sudo unavailable. Compensating control:
  stat shows socket mode 0600 and daemon-owned; kernel enforces.
- D85, constant-time token comparison: not observable black-box. Compensating
  control: all functional outcomes asserted (none/wrong/non-Bearer refused,
  correct accepted, non-loopback TLS/plaintext rules).

## Findings

1. MEDIUM (process): several Acceptance lines were reworded after the fact (D4
   drops live vendors and Codex header, D43 narrows to billing-class errors, D60
   narrows Codex, D35 excludes bare TCP reset, D87 now requires explicit
   ui.bind, D88 principals, plus a D-level library-scope rewrite of the
   allowlist item). Each rewording is justified by an unobservable surface or by
   a spec/product mismatch and is backed by a negative control, so I accept
   them, but the owner should confirm these rewordings reflect intended product
   behavior and not accommodation of a defect (especially D87 and D43 403
   semantics).
2. LOW (D12): error-kind names for wrong aud/namespace are product-chosen; spec
   does not pin them.
3. LOW (D55): printed config was adapted for the harness (server_name,
   127.0.0.1).
4. LOW (D81): retention rendered "30 days" rather than "30d"; asserted by regex
   that accepts both.
5. LOW (ef.mjs:421): one ok(true) control, informational only; not load-bearing.
6. RESOLVED since last review: D4 OAuth Bearer, D43 403 negative, D35 TCP-reset
   negative, D71 sub-cases, D75 SIGKILL, D80 real assertion, D45 note, D108
   token redaction (secret scan: 57 artifacts clean).

## Verdict rationale

Every deliverable is either directly proven by assertions that would fail if the
feature were broken, or its remaining gap is an explicitly stated black-box
exclusion with a compensating control. PASS, conditional on the owner confirming
finding 1.
