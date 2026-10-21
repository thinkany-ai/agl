#!/usr/bin/env python3
"""A scripted Claude Code (`claude -p --input-format stream-json`) for agl's
tests. Prompt keywords: TOOL asks `can_use_tool` and reports the decision;
anything else is echoed as streamed text deltas."""
import json
import sys
import uuid

session = None
for i, a in enumerate(sys.argv):
    if a in ("--session-id", "--resume") and i + 1 < len(sys.argv):
        session = sys.argv[i + 1]
session = session or str(uuid.uuid4())


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def delta(text):
    send({"type": "stream_event", "parent_tool_use_id": None,
          "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": text}}})


def result(text, error=False):
    send({"type": "result", "subtype": "error_during_execution" if error else "success", "is_error": error,
          "result": text, "session_id": session, "usage": {"input_tokens": 1, "output_tokens": 1},
          "total_cost_usd": 0.0, "duration_ms": 1, "num_turns": 1})


lines = iter(sys.stdin)
sent_init = False
for line in lines:
    msg = json.loads(line)
    if msg.get("type") == "control_request":
        sub = msg["request"].get("subtype")
        if sub == "initialize":
            send({"type": "control_response", "response": {"subtype": "success", "request_id": msg["request_id"],
                  "response": {"models": [{"value": "mock-model", "displayName": "Mock"}]}}})
        else:
            send({"type": "control_response", "response": {"subtype": "success", "request_id": msg["request_id"]}})
        continue
    if msg.get("type") != "user":
        continue
    if not sent_init:
        send({"type": "system", "subtype": "init", "session_id": session, "model": "mock-model"})
        sent_init = True
    text = "".join(b.get("text", "") for b in msg["message"]["content"])
    if "TOOL" in text:
        send({"type": "assistant", "parent_tool_use_id": None, "message": {"content": [
            {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "touch x"}}]}})
        send({"type": "control_request", "request_id": "perm-1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "touch x"}, "description": "touch x"}})
        reply = json.loads(next(lines))
        behavior = reply["response"]["response"]["behavior"]
        send({"type": "user", "parent_tool_use_id": None, "message": {"content": [
            {"type": "tool_result", "tool_use_id": "tu1", "is_error": behavior != "allow", "content": behavior}]}})
        delta(behavior)
        result(behavior)
    else:
        delta("echo: ")
        delta(text)
        result("echo: " + text)
