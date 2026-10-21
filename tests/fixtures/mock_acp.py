#!/usr/bin/env python3
"""A scripted ACP agent for agl's tests: no model, no account.

Prompt keywords:
  TOOL      ask for permission, then report the decision
  SLOW      stream until cancelled (or 30s)
  WAIT      answer "waited" after one second
  REMEMBER  REMEMBER <word> stores a word for the session (persisted, so
            resume can be tested across processes)
  RECALL    answers with the remembered word
  anything else: "echo: <prompt>" in two chunks
State lives in $MOCK_STATE_DIR (one file per session id).
"""
import json
import os
import queue
import sys
import threading
import time

STATE = os.environ.get("MOCK_STATE_DIR", "/tmp")
out_lock = threading.Lock()
replies = {}            # our request id -> queue for the client's reply
cancelled = set()
next_id = [1000]
sessions = {}


def send(obj):
    with out_lock:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


def update(sid, u):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}})


def chunk(sid, text):
    update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})


def ask(method, params):
    next_id[0] += 1
    rid = next_id[0]
    q = queue.Queue()
    replies[rid] = q
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    return q.get()


def mem_path(sid):
    return os.path.join(STATE, f"mock-acp-{sid}.txt")


def prompt(rid, p):
    sid = p["sessionId"]
    text = " ".join(b.get("text", "") for b in p.get("prompt", []))
    stop = "end_turn"
    if "TOOL" in text:
        update(sid, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "touch file", "kind": "edit", "status": "pending", "rawInput": {"path": "x.txt"}})
        r = ask("session/request_permission", {
            "sessionId": sid,
            "toolCall": {"toolCallId": "t1", "title": "touch file", "kind": "edit", "rawInput": {"path": "x.txt"}},
            "options": [
                {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                {"optionId": "always", "name": "Always", "kind": "allow_always"},
                {"optionId": "no", "name": "Reject", "kind": "reject_once"},
            ],
        })
        outcome = r.get("result", {}).get("outcome", {})
        allowed = outcome.get("outcome") == "selected" and outcome.get("optionId") in ("yes", "always")
        update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed" if allowed else "failed",
                     "content": [{"type": "content", "content": {"type": "text", "text": "touched" if allowed else "rejected"}}]})
        chunk(sid, "allowed" if allowed else "denied")
    elif "SLOW" in text:
        for i in range(150):
            if sid in cancelled:
                stop = "cancelled"
                break
            chunk(sid, f"{i} ")
            time.sleep(0.2)
    elif "WAIT" in text:
        time.sleep(1)
        chunk(sid, "waited")
    elif text.startswith("REMEMBER "):
        with open(mem_path(sid), "w") as f:
            f.write(text.split(" ", 1)[1].strip())
        chunk(sid, "ok")
    elif "RECALL" in text:
        try:
            chunk(sid, open(mem_path(sid)).read())
        except FileNotFoundError:
            chunk(sid, "nothing")
    else:
        update(sid, {"sessionUpdate": "plan", "entries": [{"content": "reply", "status": "in_progress"}]})
        chunk(sid, "echo: ")
        chunk(sid, text)
        update(sid, {"sessionUpdate": "usage_update", "used": 10, "size": 1000})
    cancelled.discard(sid)
    send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": stop}})


def main():
    counter = [0]
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        method, rid, p = msg.get("method"), msg.get("id"), msg.get("params") or {}
        if method is None:  # a reply to one of our requests
            q = replies.pop(rid, None)
            if q:
                q.put(msg)
            continue
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True, "sessionCapabilities": {"resume": {}}}}})
        elif method == "session/new":
            counter[0] += 1
            sid = f"mock-{os.getpid()}-{counter[0]}"
            sessions[sid] = True
            send({"jsonrpc": "2.0", "id": rid, "result": {
                "sessionId": sid,
                "modes": {"currentModeId": "default", "availableModes": [{"id": "default"}, {"id": "yolo"}, {"id": "plan"}]},
                "configOptions": [{"id": "model", "category": "model", "type": "select", "currentValue": "m1",
                                   "options": [{"value": "m1", "name": "Model One"}, {"value": "m2", "name": "Model Two"}]}],
            }})
        elif method in ("session/resume", "session/load"):
            send({"jsonrpc": "2.0", "id": rid, "result": {}})
        elif method in ("session/set_mode", "session/set_config_option", "session/close", "session/delete"):
            send({"jsonrpc": "2.0", "id": rid, "result": {}})
        elif method == "session/prompt":
            threading.Thread(target=prompt, args=(rid, p), daemon=True).start()
        elif method == "session/cancel":
            cancelled.add(p.get("sessionId"))
        elif rid is not None:
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unknown"}})


main()
