"""A tiny stdio MCP server for tests (standard library only).

Tools: echo, env (is GROUPER_TOKEN visible?), cwd, crash (exits mid-call), sleep (honours
notifications/cancelled), stubborn (ignores notifications/cancelled), cancelled (ids of
cancelled requests so far), and set_tools (adds/removes dynamic tools and sends
notifications/tools/list_changed).
"""

import json
import os
import sys
import threading
import time

lock = threading.Lock()
cancelled = []
events = {}
extra_tools = []


def send(msg):
    with lock:
        sys.stdout.write(json.dumps(msg) + "\n")
        sys.stdout.flush()


def text(t, error=False):
    return {"content": [{"type": "text", "text": t}], "isError": error}


def tool(name, props=None):
    return {"name": name, "description": "test " + name,
            "inputSchema": {"type": "object", "properties": props or {}}}


BASE_TOOLS = [
    tool("echo", {"text": {"type": "string"}}),
    tool("env"),
    tool("cwd"),
    tool("crash"),
    tool("sleep", {"seconds": {"type": "number"}}),
    tool("stubborn", {"seconds": {"type": "number"}}),
    tool("cancelled"),
    tool("set_tools", {"extra": {"type": "array", "items": {"type": "string"}}}),
]


def current_tools():
    with lock:
        extras = [tool(n, {"text": {"type": "string"}}) for n in extra_tools]
    return BASE_TOOLS + extras


def call(req_id, name, args):
    if name == "echo":
        return text(args.get("text", ""))
    if name == "env":
        return text("GROUPER_TOKEN=" + ("present" if "GROUPER_TOKEN" in os.environ else "absent")
                    + " pid=" + str(os.getpid()))
    if name == "cwd":
        return text(os.getcwd())
    if name == "crash":
        os._exit(3)
    if name == "sleep":
        ev = threading.Event()
        events[req_id] = ev
        ev.wait(float(args.get("seconds", 1)))
        return text("cancelled" if req_id in cancelled else "slept")
    if name == "stubborn":
        time.sleep(float(args.get("seconds", 30)))
        return text("finished")
    if name == "cancelled":
        return text(json.dumps(cancelled))
    if name == "set_tools":
        names = [str(n) for n in (args.get("extra") or [])]
        with lock:
            extra_tools[:] = names
        send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        return text(json.dumps(names))
    with lock:
        is_extra = name in extra_tools
    if is_extra:
        return text(args.get("text", name))
    return text("unknown tool " + name, True)


def handle_call(req_id, params):
    send({"jsonrpc": "2.0", "id": req_id, "result": call(req_id, params.get("name"), params.get("arguments") or {})})


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": req_id, "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "grouper-test", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"tools": current_tools()}})
    elif method == "tools/call":
        threading.Thread(target=handle_call, args=(req_id, msg.get("params") or {}), daemon=True).start()
    elif method == "notifications/cancelled":
        rid = (msg.get("params") or {}).get("requestId")
        cancelled.append(rid)
        if rid in events:
            events[rid].set()
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": req_id, "result": {}})
    elif req_id is not None:
        send({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "no " + str(method)}})
