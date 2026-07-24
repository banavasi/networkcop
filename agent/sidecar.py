#!/usr/bin/env python3
"""
A drop-in replacement reasoner for networkcop, built on LangGraph.

networkcop's Rust binary drives `claude -p` by default. Point it here instead to
swap the reasoning engine without touching the capture pipeline or the guardrail:

    pip install -r requirements.txt
    export OPENAI_API_KEY=...          # or ANTHROPIC_API_KEY
    python sidecar.py                  # listens on :8099

    networkcop 8080 --sidecar http://127.0.0.1:8099

Contract — this is all the Rust side needs:

    POST /ask  {"system": "<guardrail prompt>", "input": "<session + question>"}
    ->         {"result": "<raw model text>", "cost_usd": 0.0}

`result` is passed through networkcop's validator unchanged, so a sidecar that
misbehaves gets refused rather than trusted. The graph below mirrors the Rust
state machine: observe -> reason -> validate.
"""

from __future__ import annotations

import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import TypedDict

from langchain_core.messages import HumanMessage, SystemMessage
from langgraph.graph import END, StateGraph

PORT = int(os.environ.get("SIDECAR_PORT", "8099"))


def _model():
    """Whichever provider is configured. Anthropic first, then OpenAI."""
    if os.environ.get("ANTHROPIC_API_KEY"):
        from langchain_anthropic import ChatAnthropic

        return ChatAnthropic(
            model=os.environ.get("SIDECAR_MODEL", "claude-haiku-4-5-20251001"),
            temperature=0,
            max_tokens=2048,
        )
    from langchain_openai import ChatOpenAI

    return ChatOpenAI(
        model=os.environ.get("SIDECAR_MODEL", "gpt-4o-mini"),
        temperature=0,
    )


class State(TypedDict):
    system: str
    input: str
    raw: str
    ok: bool


def observe(state: State) -> State:
    """The session digest arrives pre-built from Rust; nothing to gather here."""
    return state


def reason(state: State) -> State:
    resp = _model().invoke(
        [SystemMessage(content=state["system"]), HumanMessage(content=state["input"])]
    )
    return {**state, "raw": resp.content if isinstance(resp.content, str) else str(resp.content)}


def validate(state: State) -> State:
    """
    Mirror of the Rust validator. Belt and braces: Rust validates again on receipt,
    so a bug here cannot widen the guardrail — it can only refuse earlier.
    """
    text = state.get("raw", "").strip()
    for fence in ("```json", "```"):
        if text.startswith(fence):
            text = text[len(fence):]
    text = text.rstrip("`").strip()

    start, end = text.find("{"), text.rfind("}")
    if start == -1 or end <= start:
        return {**state, "ok": False}
    try:
        parsed = json.loads(text[start : end + 1])
    except json.JSONDecodeError:
        return {**state, "ok": False}

    ok = bool(parsed.get("in_scope")) and bool(str(parsed.get("answer", "")).strip())
    return {**state, "raw": json.dumps(parsed), "ok": ok}


def build_graph():
    g = StateGraph(State)
    g.add_node("observe", observe)
    g.add_node("reason", reason)
    g.add_node("validate", validate)
    g.set_entry_point("observe")
    g.add_edge("observe", "reason")
    g.add_edge("reason", "validate")
    g.add_edge("validate", END)
    return g.compile()


GRAPH = build_graph()


class Handler(BaseHTTPRequestHandler):
    def _send(self, code: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self._send(200, {"ok": True})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/ask":
            self._send(404, {"error": "not found"})
            return
        try:
            n = int(self.headers.get("Content-Length", "0"))
            req = json.loads(self.rfile.read(n) or b"{}")
        except (ValueError, json.JSONDecodeError) as e:
            self._send(400, {"error": f"bad request: {e}"})
            return

        try:
            out = GRAPH.invoke(
                {
                    "system": req.get("system", ""),
                    "input": req.get("input", ""),
                    "raw": "",
                    "ok": False,
                }
            )
            # Always return the raw envelope; Rust decides whether to show it.
            self._send(200, {"result": out.get("raw", ""), "cost_usd": 0.0})
        except Exception as e:  # a dead provider must not take the TUI down
            self._send(200, {"result": "", "cost_usd": 0.0, "error": str(e)})

    def log_message(self, *_args) -> None:
        """Quiet — the TUI owns the terminal."""


if __name__ == "__main__":
    print(f"networkcop sidecar listening on http://127.0.0.1:{PORT}")
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
