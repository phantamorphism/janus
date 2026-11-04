---
title: Governance
type: reference
status: accepted
---

# Governance

Janus is a small project with a benevolent-dictator model, kept light on
purpose. This document says who decides, how, and where decisions are recorded.

## Roles

- **Maintainer.** Currently the repository owner,
  [@phantamorphism](https://github.com/phantamorphism). A maintainer can merge,
  cut releases, and handle security reports. Anyone who has contributed
  sustained, careful work, especially on the trust boundary, can be invited to
  become a maintainer; the invitation is the maintainer's call.
- **Contributor.** Anyone who opens an issue, a pull request or a review.

## How decisions are made

1. **Routine changes** (bug fixes, tests, docs, refactors that do not change
   behaviour) are decided in pull-request review by a maintainer.
2. **Design changes** (new upstreams, identity methods, config keys, admin
   endpoints, anything on the trust boundary, any change to `proto/janus.proto`)
   are proposed in an issue first. Discussion is open and aims for rough
   consensus. Where it does not arrive, the maintainer decides and gives the
   reason.
3. **The bar is the project's goals**, in `docs/index.md`: callers never hold
   real credentials, every call is a policy enforcement point, standalone by
   default, extensible through the plugin contract rather than forks. A proposal
   is judged on fit with those goals and on whether a real user needs it, with
   correctness preferred over ease of implementation and nothing added without a
   user behind it.
4. **Security-sensitive changes** are held to a higher standard of evidence: a
   test for the refusal path and verification against a real daemon (see
   `docs/development.md`).

## Where decisions are recorded

`docs/decisions.md` holds the decision records: each settled design question,
the decision, how it was reached, and, for contested ones, the options
considered and why they were rejected. Its purpose is to let a later reader tell
a settled choice from an accident. When a design issue is resolved, the outcome
is added there (through the `sch` workflow) and the design docs are updated to
match. A decision is reopened by a new issue that argues from new information,
not by repeating the original request. `docs/status.md` lists what is
deliberately not built, each with the trigger that would justify building it.

## Changing this document

Changes to governance are proposed as a pull request and decided by the
maintainer after open discussion. If the project grows past what one maintainer
can steward, this document is the place to add a steering group.
