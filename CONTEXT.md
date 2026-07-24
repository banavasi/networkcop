# CONTEXT — networkcop

Durable project context. Read this and `docs/adr/` before working here.

## What this is

`networkcop <port>` launches Chrome via the DevTools Protocol against a local dev
server, records the entire debugging session to SQLite, renders a four-pane Ratatui
TUI, and exposes an agent that may only reason about the captured session.

Repo directory is `cdpmon`; the crate, binary and GitHub repo are all `networkcop`.

## Shape

```
src/
  main.rs        CLI, event loop, capture→DB wiring, shutdown
  cdp/mod.rs     Chrome launch + CDP websocket (one task owns the socket)
  cdp/proto.rs   the slice of CDP we consume
  db.rs          SQLite schema + store; the agent's entire world
  app.rs         pane focus, filtering, input buffer — no ratatui types, so testable
  tui/           overview · network (+ detail modal) · console · chat
  agent/
    mod.rs       Observe → Reason → Act → Persist; intent classification
    prompt.rs    the hard-coded guardrail + the fix-prompt template
    llm.rs       claude -p driver + the output validator
    tools.rs     deterministic tools: OpenAPI, save-page, curl/Playwright, Jira
examples/
  probe.rs       Phase 1 spike — CDP body-capture window
  guard.rs       Phase 2 spike — guardrail corpus + cost
agent/sidecar.py LangGraph alternative reasoner
```

## Load-bearing facts

Both were measured, not assumed. Re-run the spikes if you change the relevant code.

1. **Response bodies do not survive navigation.** `probe.rs` measured 0/554 bodies
   retrievable after one navigation, 554/554 at `loadingFinished`. Bodies are fetched
   eagerly, concurrency-limited to 16, size-capped at `--max-body` (2 MiB), through a
   bounded channel. There is no lazy path to fall back on.
2. **The guardrail prompt holds.** `guard.rs` measured 8/8 off-scope refusals
   (including a prompt injection) and 7/7 in-scope answers on haiku, at
   $0.0234/turn with `--exclude-dynamic-system-prompt-sections --strict-mcp-config`
   (41% cheaper than without).

A real page load produces ~560 requests. Anything on the capture path is hot; the
agent digest filters to API-shaped calls plus all errors before building a prompt.

## Verification

```bash
cargo test                                    # 51 tests, no network
cargo clippy --all-targets -- -D warnings
cargo build --release                         # must be warning-free
./target/release/networkcop --dump-layout     # pane geometry as JSON
networkcop sessions --json                    # session index
cargo publish --dry-run
```

`sqlite3` and `asciinema` are NOT assumed installed — they are absent on the dev
machine. Session inspection goes through `networkcop sessions --json` and layout
verification through `--dump-layout`, so every check runs against the binary.

The TUI needs a pty. To exercise it non-interactively:
`script -qec "./target/release/networkcop 8080 --headless --db /tmp/t.db" /dev/null`,
then `kill -INT` the process — SIGINT and SIGTERM both route to the flush path.

## Not done yet

- Not published to crates.io (needs `cargo login`; no token on this machine).
- No GitHub remote yet — `scripts/setup-github.sh --public` creates it.
- A parallel cloud implementation of the same spec may arrive as a PR; diff before
  merging rather than assuming either side is canonical.
