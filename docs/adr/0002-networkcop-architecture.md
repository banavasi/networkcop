# 0002 — networkcop architecture

- Status: **accepted**
- Date: 2026-07-24
- Supersedes: none

## Context

`networkcop <port>` launches Chrome against a local dev server, records the whole
debugging session (network + console + navigations) to SQLite, renders a four-pane
Ratatui TUI, and exposes an agent that may only reason about that session.

Two questions could have invalidated the design. Both were settled by throwaway
spikes (`examples/probe.rs`, `examples/guard.rs`) before any real code was written.

## Spike 1 — do response bodies survive a navigation?

`cargo run --example probe -- 8080` against a live Vite dev app, Chrome 150.0.7871.128:

```
bodies at loadingFinished : 554/554
bodies after navigation   : 0/554
VERDICT → body_after_nav: EVICTED
```

`Network.getResponseBody` fails for **every** prior request once the page navigates,
with `No resource with given identifier found`. Lazy fetch-on-open is not viable.

Two further facts the spike surfaced:

- One page load of a real app produced **554 requests**. The capture path is hot.
- The largest single body was **12.3 MB** (a hero video); several third-party
  bundles exceeded 890 kB.

### Decision

Fetch every body **eagerly**, inside the `Network.loadingFinished` handler, and hand
it to a bounded channel feeding a dedicated SQLite writer task.

- Bodies larger than `--max-body` (default **2 MiB**) are recorded with their true
  size and a `truncated` flag rather than stored whole.
- The channel is bounded (1024). The CDP reader awaits capacity rather than
  unbounded-buffering, so a chatty app slows capture instead of exhausting memory.
- Body fetches are issued concurrently with a semaphore (16) so one 12 MB body does
  not stall the 553 requests behind it.

### Consequences

- "Full headers + bodies" is honest for everything under the cap, and explicitly
  flagged for what exceeds it.
- Capture cost is paid up front, per request, whether or not anyone opens the row.

## Spike 2 — does the guardrail hold, and what does a turn cost?

`cargo run --example guard`, model `haiku`, 16-prompt corpus (8 in-scope, 8 out):

```
in-scope answered : 7/7
off-scope refused : 8/8      (incl. a direct prompt-injection attempt)
parse failures    : 1        (/reproduce — see below)
mean_cost_usd     : 0.02340
```

Flag A/B on an identical prompt:

```
plain --system-prompt        $0.04331
--exclude-dynamic + strict   $0.02552     (-41%)
```

### Decision

- Reasoning runs through the **`claude` CLI** (`claude -p --output-format json`) on
  the user's existing subscription — no API key, per profile pref
  `build:claude-subscription-llm`. Reference pattern: `voice-mentor/src/summary.rs`.
- Always pass `--exclude-dynamic-system-prompt-sections --strict-mcp-config` and a
  `--disallowed-tools` list. The agent reasons; it never touches disk or network.
- Guardrails are **two layers**, and the spike shows the first one carries its weight:
  1. the hard-coded system prompt (8/8 refusals, injection included);
  2. a Rust envelope validator — the model must return
     `{"in_scope":bool,"answer":str}`, and anything that fails to parse, or parses
     with `in_scope:false`, is replaced by the canned refusal. The model's prose is
     never shown unvalidated.
- **Slash commands never take the envelope path.** The one parse failure was
  `/reproduce` returning a bash block. That is the correct instinct wearing the wrong
  costume: commands are deterministic Rust functions that read SQLite directly. The
  LLM is invoked only for the prose slots inside them (a bug description, a branch
  slug), never to produce the artifact.

### Consequences

- ~2.3¢ per free-form turn on haiku. Slash commands that need no prose cost nothing.
- The refusal is a constant string, so it cannot itself be prompt-injected.
- Swapping the reasoner (the Python LangGraph sidecar under `agent/`) changes only
  which subprocess is spawned; the validator is unconditional.

## Persistence

SQLite at `~/.networkcop/sessions.db` (override with `--db`), WAL mode, one row per
request/response/console entry/navigation, plus user annotations. Loaded on startup
so prior sessions remain queryable.

`sqlite3` is **not** assumed to be installed — it is absent on the development
machine. Session inspection ships as `networkcop sessions --json`, so every
verification step runs against the binary itself.

## Milestone checks that changed

- M3 uses `networkcop sessions --json`, not the `sqlite3` CLI (absent).
- M4 uses `networkcop --dump-layout` plus a canned-event unit test, not an
  asciinema recording (`asciinema` absent, and a human watching panes render is not
  a check).
