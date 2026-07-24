# cdpmon — project memory (profile: personal)
> Precedence: PROJECT (this file) > PROFILE > STACK > GLOBAL — this file wins. See ~/.claude/CLAUDE.md.
> Project preferences below; managed by /pref (don't hand-edit the fence).

<!-- PREFS:PROJECT:START -->
### build
- Settle every load-bearing unknown with a throwaway spike under `examples/` that prints a explicit verdict line, and record the resulting numbers in an ADR under `docs/adr/`, BEFORE writing the real implementation. Both of this project's spikes changed the design: `probe.rs` measured 0/554 response bodies retrievable after a navigation (killing lazy fetch), and `guard.rs` measured the guardrail's actual refusal rate and per-turn cost. Keep the spikes runnable and re-run them when the code they measured changes.  <!-- key:build:spike-before-build -->
<!-- PREFS:PROJECT:END -->
