---
title: Reviews
type: reference
status: accepted
---

# Reviews

Independent review reports on Janus, kept in the repository as a record of what
was examined and what was found. Each review was done by a separate reviewer
with no part in the implementation, working from the code and the design docs
and running the checks themselves; each report opens with a verdict and states
its scope, the commands it ran, and its findings with severities.

| Report | Subject |
| --- | --- |
| `contract-1.1.1.md` | The contract scaffold: `janus-core`'s contract module, the daemon's contract module and the crate manifests. |
| `core.md` | `janus-core`, plus the daemon's plugin client and storage layers. |
| `daemon.md` | `janus-daemon`: the data plane, admin surface and listeners. |
| `clients.md` | The `janus` CLI and the UI. |
| `architecture.md` | The whole workspace against the architecture docs. |
| `deliverables.md` | `evidence/DELIVERABLES.md` checked against the design docs and `proto/janus.proto`. |
| `evidence-assessment.md` | An assessment of the acceptance evidence in <ref target="evidence/README">evidence</ref> against each deliverable. |

Several reports are from a later round after earlier findings were fixed; a
report records the state it reviewed, so a finding listed there may since have
been resolved. The current build state is in
<ref target="docs/status">status</ref>, and the acceptance results are in
`evidence/results.json`.
