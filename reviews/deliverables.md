---
title: deliverables
type: reference
status: draft
---

VERDICT: PASS REVIEWER: independent-deliverables-reviewer (claude-opus-5-5),
round 3 (final)

# Review of evidence/DELIVERABLES.md against docs/*.md and proto/janus.proto

## Scope

This round re-read the whole of evidence/DELIVERABLES.md (D1 to D110, plus the
reading note on line 26) against the current spec.

The current spec is:
- docs/*.md, including the amended caller-identity.md (hmac wire format) and
  architecture.md (three launch upstreams);
- proto/janus.proto.

The same four checks as before were applied:
1. Nothing invented.
2. Nothing omitted.
3. Every Acceptance line can be decided by a blind tester.
4. The file is self-contained.

DELIVERABLES.md was not edited by this reviewer.

## Change check since round 2

- **DELIVERABLES.md:** a full re-read found every deliverable other than D71
  word-for-word identical to the round-2 text.
- **Docs and proto:** line counts are unchanged from round 2 (admin-surface 288,
  architecture 328, caller-identity 353, credential-substitution 163, decisions
  236, index 96, plugin-contract 752, scope 106, status 107, janus.proto 120).
- **Conclusion:** no new invented values and no new omissions.

## Coverage walk (doc by doc, unchanged from round 2 and re-confirmed)

- **proto/janus.proto**
  - ResolveRoute fields and outcomes: D28 to D31.
  - Fail-closed behaviour: D32 to D36.
  - Unset oneof: D34.
  - Notify envelope: D45.
  - The four guaranteed events: D38 to D43.
  - Empty-identity advice: D37.
- **index.md**
  - Goals: D9, D64, D31, D27, D57.
  - The standalone assumption is stated on line 26 and exercised in D27.
  - The three upstreams: D2.
- **credential-substitution.md**
  - Substitution: D1, D3 to D5.
  - The caller never learns the credential: D9.
  - Policy point: D21, D31.
  - Pass-through: D27.
  - Per-upstream specifics: D3, D4, D5, D38, D60.
- **caller-identity.md**
  - Methods and the hmac wire format: D10 to D15.
  - Sources: D16.
  - Binding rules: D17 to D20, D84, D103.
  - Correlation: D25.
  - Scrub: D5, D6.
  - Failure posture: D21 to D23.
  - Custom extractors: D26.
  - Registry: D13, D14, D93, D94, D110.
- **architecture.md**
  - Listeners: D82 to D87.
  - Lifecycle steps 1 to 7: D1, D5, D7, D8, D21, D28 to D31, D38 to D44, D60,
    D61.
  - Proxy behaviours and out-of-scope items: D7, D44, D80.
- **plugin-contract.md**
  - ResolveRoute and Notify: D28 to D46.
  - Transports and config: D47 to D50.
  - Channel security: D51 to D56.
  - Linked plugins and the custom build: D36, D57, D58.
  - Metering: D65 to D77.
- **admin-surface.md**
  - Endpoints: D88 to D96.
  - Request log: D78 to D81.
  - CLI: D97 to D103.
  - UI: D87, D104 to D109.
  - Storage: D110.
- **decisions.md**: every decision maps to a design deliverable above.
- **scope.md**: D2, D59, D60, D78, D90, D95, D96.
- **status.md**: no behavioural claims.

## Findings

- [x] [low] D71 (b) to (d) didn't state the quota window, which let compaction
  of counter keys interfere with the ledger-TTL test. Resolved and verified: (b)
  to (d) now run on a meter whose only quota is `Fixed { period: 24 h }`. A
  key's `retain_until` therefore can't pass at `compact(T + 59 min)` or
  `compact(T + 61 min)`, so only ledger-TTL expiry is exercised. The expected
  results follow from plugin-contract.md#usage-stores, given the default
  one-hour TTL:
  - (b) refund `true`;
  - (c) compact drops more than zero entries, refund `false`, and the charge
    stands;
  - (d) the redelivered record counts again.
- [x] Every round-1 finding stays resolved. These were the six mediums (D12,
  D36, D79, D105, the D10/D11 wire format, untested defaults) and fifteen lows.
  The round-2 re-verification still holds because the text of those deliverables
  is unchanged.

No open findings.
