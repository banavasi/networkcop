# 0001 — Record architecture decisions

- Status: **accepted**

## Context

`cdpmon` records significant decisions as ADRs (numbered `NNNN-<slug>.md` in this
directory). ADRs plus `CONTEXT.md` are the project's durable, tool-independent memory —
the context an agent loads before working (devskill ADR-0006/0013). This is required in
every project; a task tracker (Linear/Jira/…) is optional and pluggable (ADR-0013).

## Decision

Use lightweight ADRs (context → decision → consequences). One decision per file, append
-only, never rewrite an accepted ADR — supersede it with a later one that references it.

## Consequences

- Decisions are reviewable in git and travel with the repo.
- `CONTEXT.md` + `docs/adr/` are load-bearing runtime context, not just documentation.
