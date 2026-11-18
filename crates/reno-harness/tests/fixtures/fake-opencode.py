#!/usr/bin/env python3
"""A fake `opencode` for the driver's tests: `--version` and `serve --port N --hostname H`.

It is an HTTP and SSE server speaking the 1.x wire (`/global/health`, `/session`, `prompt_async`,
`/global/event`) or the 2.x wire (`/api/info`, `/api/session`, `/api/event`), by the version it is
told to be (FAKE_OPENCODE_VERSION, default 1.19.5). The route shapes and the frame shapes are the
ones read off real servers (1.19.5 and 2.3.2) and zeron's recordings; see `drivers::opencode`.

It behaves like the real one where the driver depends on it: HTTP Basic auth with
OPENCODE_SERVER_PASSWORD (401 otherwise), the web UI answering 200 HTML on a route of the other
generation, an agent whose permission rules come from OPENCODE_CONFIG_CONTENT (unless
FAKE_OPENCODE_RULES=ignore), MCP servers listed from it (unless FAKE_OPENCODE_MCP=none), a bus
with no replay, and heartbeats.

The scenario a turn plays is named by its prompt (`scenario:<name>`). Everything the fake gets is
appended to FAKE_OPENCODE_LOG as JSON lines (`argv`, `env`, `req`, `event`), and its sessions are
kept under FAKE_OPENCODE_ROOT so a restarted server finds them.
"""

import base64
import json
import os
import queue
import re
import signal
import subprocess
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

VERSION = os.environ.get("FAKE_OPENCODE_VERSION", "1.19.5")
V2 = int(VERSION.split(".")[0]) >= 2
LOG = os.environ.get("FAKE_OPENCODE_LOG")
ROOT = os.environ.get("FAKE_OPENCODE_ROOT") or os.path.join(os.path.dirname(LOG or "/tmp/x"), "fake-root")
PASSWORD = os.environ.get("OPENCODE_SERVER_PASSWORD", "")
HEARTBEAT = float(os.environ.get("FAKE_OPENCODE_HEARTBEAT", "2"))
LOCK = threading.Lock()


def log(entry):
    if not LOG:
        return
    with LOCK:
        with open(LOG, "a") as handle:
            handle.write(json.dumps(entry) + "\n")


if "--version" in sys.argv[1:2] or sys.argv[1:2] == ["-v"]:
    print(("opencode v" + VERSION) if V2 else VERSION)
    sys.exit(0)

if sys.argv[1:2] != ["serve"]:
    sys.stderr.write("usage: opencode serve --port N --hostname H\n")
    sys.exit(2)


def arg(name, default=None):
    if name in sys.argv:
        return sys.argv[sys.argv.index(name) + 1]
    return default


PORT = int(arg("--port", "0"))
HOST = arg("--hostname", "127.0.0.1")
CONFIG = os.environ.get("OPENCODE_CONFIG_CONTENT")
log({
    "argv": sys.argv[1:],
    "pid": os.getpid(),
    "cwd": os.getcwd(),
    "env": {
        key: os.environ.get(key)
        for key in (
            "OPENCODE_DISABLE_AUTOUPDATE", "OPENCODE_ENABLE_QUESTION_TOOL", "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_DISABLE_MODELS_FETCH", "OPENCODE_CLIENT", "OPENCODE_SERVER_USERNAME",
            "OPENCODE_CONFIG", "OPENCODE_PERMISSION", "ANTHROPIC_API_KEY", "GITHUB_TOKEN", "FAKE_RUN_SECRET",
            "RENO_RUN_ID", "HOME",
        )
        if os.environ.get(key) is not None
    },
    "password_set": bool(PASSWORD),
    "password_in_argv": bool(PASSWORD) and any(PASSWORD in a for a in sys.argv),
})

STARTUP = os.environ.get("FAKE_OPENCODE_STARTUP", "")
if STARTUP == "exit":
    sys.stderr.write("Error: cannot start. Authorization: Bearer sk-secretvalue123 rejected\n")
    sys.exit(1)
if STARTUP == "locked-once":
    marker = os.path.join(ROOT, "locked-once")
    os.makedirs(ROOT, exist_ok=True)
    if not os.path.exists(marker):
        open(marker, "w").close()
        sys.stderr.write("Error: SQLITE_BUSY: database is locked\n")
        sys.exit(1)
if STARTUP.startswith("slow:"):
    time.sleep(float(STARTUP.split(":")[1]))

os.makedirs(os.path.join(ROOT, "sessions"), exist_ok=True)


# ---- the bus -----------------------------------------------------------------------------------

class Bus:
    """No replay: a frame goes to the subscribers there are when it is emitted."""

    def __init__(self):
        self.subscribers = []
        self.lock = threading.Lock()

    def subscribe(self):
        q = queue.Queue()
        with self.lock:
            self.subscribers.append(q)
        return q

    def drop(self, q):
        with self.lock:
            if q in self.subscribers:
                self.subscribers.remove(q)

    def count(self):
        with self.lock:
            return len(self.subscribers)

    def send(self, frame):
        with self.lock:
            for q in self.subscribers:
                q.put(frame)

    def close_all(self):
        with self.lock:
            for q in self.subscribers:
                q.put(None)


BUS = Bus()
EVENT_SEQ = [0]


def frame_v1(kind, props):
    EVENT_SEQ[0] += 1
    return {"payload": {"id": "evt_%d" % EVENT_SEQ[0], "type": kind, "properties": props}}


def frame_v2(kind, data):
    EVENT_SEQ[0] += 1
    return {"id": "evt_%d" % EVENT_SEQ[0], "type": kind, "data": data}


def emit(kind, **a):
    """One abstract event, rendered as the frames of the generation this server speaks."""
    log({"event": kind, **{k: v for k, v in a.items() if k in ("sid", "text", "name", "attempt")}})
    frames = render_v2(kind, a) if V2 else render_v1(kind, a)
    for f in frames:
        BUS.send(f)
    time.sleep(0.005)


def render_v1(kind, a):
    sid = a.get("sid")
    if kind == "busy":
        return [frame_v1("session.status", {"sessionID": sid, "status": {"type": "busy"}})]
    if kind == "idle":
        return [
            frame_v1("session.status", {"sessionID": sid, "status": {"type": "idle"}}),
            frame_v1("session.idle", {"sessionID": sid}),
        ]
    if kind == "interrupted":
        return [frame_v1("session.interrupted", {"sessionID": sid})]
    if kind == "message":
        return [frame_v1("message.updated", {"info": {"sessionID": sid, "id": a["mid"], "role": a.get("role", "assistant")}})]
    if kind == "text":
        return [frame_v1("message.part.updated", {"part": {
            "sessionID": sid, "id": a["pid"], "messageID": a["mid"], "type": a.get("ptype", "text"), "text": a["text"]}})]
    if kind == "delta":
        return [frame_v1("message.part.delta", {
            "sessionID": sid, "messageID": a["mid"], "partID": a["pid"], "field": "text", "delta": a["text"]})]
    if kind == "tool":
        state = {"status": a["status"], "input": a.get("input", {})}
        if a["status"] == "completed":
            state["output"] = a.get("output", "")
        if a["status"] == "error":
            state["error"] = a.get("output", "")
        return [frame_v1("message.part.updated", {"part": {
            "sessionID": sid, "id": a["pid"], "callID": a["call"], "messageID": a["mid"],
            "type": "tool", "tool": a["name"], "state": state}})]
    if kind == "usage":
        info = {"sessionID": sid, "id": a["mid"], "role": "assistant", "tokens": a["tokens"],
                "providerID": "opencode", "modelID": "big-pickle", "time": {"completed": 1}}
        return [frame_v1("message.updated", {"info": info})]
    if kind == "retry":
        return [frame_v1("session.status", {"sessionID": sid, "status": {
            "type": "retry", "attempt": a["attempt"], "message": a.get("text", "overloaded")}})]
    if kind == "error":
        return [frame_v1("session.error", {"sessionID": sid, "error": {"name": "APIError", "data": {"message": a["text"]}}})]
    if kind == "child":
        return [frame_v1("session.created", {"info": {"id": a["child"], "parentID": sid, "title": "child"}})]
    if kind == "permission":
        return [frame_v1("permission.asked", a["props"])]
    if kind == "question":
        return [frame_v1("question.asked", a["props"])]
    if kind == "unknown":
        return [frame_v1("session.teleported", {"sessionID": sid})]
    raise SystemExit("unknown event " + kind)


def render_v2(kind, a):
    sid = a.get("sid")
    mid = a.get("mid", "msg_none")
    if kind == "busy":
        return [frame_v2("session.execution.started", {"sessionID": sid})]
    if kind == "idle":
        return [
            frame_v2("session.status", {"sessionID": sid, "status": {"type": "idle"}}),
            frame_v2("session.execution.succeeded", {"sessionID": sid}),
        ]
    if kind == "interrupted":
        return [frame_v2("session.execution.interrupted", {"sessionID": sid})]
    if kind == "message":
        return [frame_v2("session.step.started", {"sessionID": sid, "assistantMessageID": mid,
                                                 "model": {"providerID": "opencode", "id": "big-pickle"}})]
    if kind == "text":
        k = "reasoning" if a.get("ptype") == "reasoning" else "text"
        return [frame_v2("session.%s.ended" % k, {"sessionID": sid, "assistantMessageID": mid, "ordinal": 0, "text": a["text"]})]
    if kind == "delta":
        return [frame_v2("session.text.delta", {"sessionID": sid, "assistantMessageID": mid, "ordinal": 0, "delta": a["text"]})]
    if kind == "tool":
        base = {"sessionID": sid, "assistantMessageID": mid, "id": a["call"]}
        if a["status"] == "running":
            return [
                frame_v2("session.tool.input.started", {**base, "name": a["name"]}),
                frame_v2("session.tool.called", {**base, "input": a.get("input", {})}),
            ]
        if a["status"] == "completed":
            return [frame_v2("session.tool.success", {**base, "content": [{"text": a.get("output", "")}]})]
        return [frame_v2("session.tool.failed", {**base, "error": {"message": a.get("output", "")}})]
    if kind == "usage":
        return [frame_v2("session.step.ended", {"sessionID": sid, "assistantMessageID": mid, "tokens": a["tokens"]})]
    if kind == "retry":
        return [frame_v2("session.retry.scheduled", {"sessionID": sid, "attempt": a["attempt"], "at": 1,
                                                    "error": {"message": a.get("text", "overloaded")}})]
    if kind == "error":
        return [frame_v2("session.execution.failed", {"sessionID": sid, "error": {"type": "provider", "message": a["text"]}})]
    if kind == "child":
        return [frame_v2("session.created", {"sessionID": a["child"], "parentID": sid, "title": "child"})]
    if kind == "permission":
        p = a["props"]
        return [frame_v2("permission.asked", {
            "id": p["id"], "sessionID": p["sessionID"], "action": p["permission"],
            "resources": p.get("patterns", []), "metadata": p.get("metadata", {})})]
    if kind == "question":
        p = a["props"]
        fields = []
        for i, q in enumerate(p["questions"]):
            fields.append({"key": "q%d" % i, "title": q.get("header", "Question"), "description": q["question"],
                           "type": "multiselect" if q.get("multiple") else "select",
                           "options": [{"label": o["label"], "value": o["label"]} for o in q.get("options", [])]})
        return [frame_v2("form.created", {"form": {"id": p["id"], "sessionID": p["sessionID"],
                                                  "metadata": {"kind": "question"}, "fields": fields}})]
    if kind == "unknown":
        return [frame_v2("session.teleported", {"sessionID": sid})]
    raise SystemExit("unknown event " + kind)


# ---- state -------------------------------------------------------------------------------------

class State:
    def __init__(self):
        self.running = {}        # session id -> True while a scenario plays
        self.aborted = {}        # session id -> Event
        self.messages = set()
        self.pending_permissions = {}   # id -> props
        self.pending_questions = {}     # id -> props
        self.replies = {}        # id -> reply payload
        self.reply_events = {}   # id -> Event
        self.prompts = 0
        self.lock = threading.Lock()


ST = State()


def session_path(sid):
    return os.path.join(ROOT, "sessions", sid + ".json")


def new_session(directory, agent):
    sid = "ses_" + uuid.uuid4().hex[:24]
    with open(session_path(sid), "w") as handle:
        json.dump({"id": sid, "directory": directory, "agent": agent}, handle)
    return sid


def load_session(sid):
    try:
        with open(session_path(sid)) as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def config_json():
    try:
        return json.loads(CONFIG) if CONFIG else {}
    except ValueError:
        return {}


def agent_rules(name):
    """The permission rules of an agent as the server reports them, the inline config layered last."""
    config = config_json()
    if os.environ.get("FAKE_OPENCODE_RULES") == "ignore":
        config = {}
    if V2:
        rules = [{"action": "*", "resource": "*", "effect": "allow"}]
        rules += config.get("permissions", [])
        rules += config.get("agents", {}).get(name, {}).get("permissions", [])
        return rules
    rules = [{"permission": "*", "pattern": "*", "action": "allow"}, {"permission": "question", "pattern": "*", "action": "deny"}]
    permission = config.get("permission", {})
    if isinstance(permission, dict):
        for key, value in permission.items():
            rules.append({"permission": key, "pattern": "*", "action": value})
    return rules


def mcp_listing():
    if os.environ.get("FAKE_OPENCODE_MCP") == "none":
        return {} if not V2 else {"data": []}
    config = config_json()
    if V2:
        servers = config.get("mcp", {}).get("servers", {})
        return {"data": [{"name": n, "status": {"status": "connected"}} for n in servers]}
    return {n: {"status": "connected"} for n in config.get("mcp", {})}


def catalog_v1():
    connected = [] if os.environ.get("FAKE_OPENCODE_CONNECTED") == "none" else ["opencode"]
    return {
        "all": [
            {"id": "opencode", "name": "OpenCode Zen", "models": {
                "big-pickle": {"name": "Big Pickle", "limit": {"context": 200000}, "variants": {}},
                "reasoner": {"name": "Reasoner", "limit": {"context": 100000},
                             "variants": {"low": {}, "high": {}}},
            }},
            {"id": "anthropic", "name": "Anthropic", "models": {"claude-x": {"name": "Claude X"}}},
        ],
        "default": {"opencode": "big-pickle"},
        "connected": connected,
    }


def catalog_v2():
    if os.environ.get("FAKE_OPENCODE_CONNECTED") == "none":
        return {"location": {}, "data": []}
    return {"location": {}, "data": [
        {"id": "big-pickle", "providerID": "opencode", "name": "Big Pickle", "limit": {"context": 200000},
         "variants": [], "enabled": True},
        {"id": "reasoner", "providerID": "opencode", "name": "Reasoner", "limit": {"context": 100000},
         "variants": [{"id": "low", "settings": {}}, {"id": "high", "settings": {}}], "enabled": True},
    ]}


AGENTS = [
    {"name": "build", "mode": "primary"}, {"name": "plan", "mode": "primary"},
    {"name": "general", "mode": "subagent"}, {"name": "title", "mode": "primary", "hidden": True},
]


# ---- scenarios -----------------------------------------------------------------------------------

def wait_reply(rid, sid, timeout=30):
    """Block until a person answers request `rid` (or the session is aborted)."""
    event = ST.reply_events.setdefault(rid, threading.Event())
    deadline = time.time() + timeout
    while time.time() < deadline:
        if event.wait(0.05):
            return ST.replies.get(rid)
        if ST.aborted.get(sid) and ST.aborted[sid].is_set():
            return None
    return None


def aborted(sid):
    return ST.aborted.get(sid) is not None and ST.aborted[sid].is_set()


def usage(sid, mid, **tokens):
    base = {"input": 10, "output": 20, "reasoning": 5, "cache": {"read": 7, "write": 3}}
    base.update(tokens)
    emit("usage", sid=sid, mid=mid, tokens=base)


def finish(sid):
    emit("idle", sid=sid)


def sc_text(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="Hello ")
    emit("delta", sid=sid, mid="msg_a1", pid="prt_t1", text="from ")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="Hello from opencode")
    emit("text", sid=sid, mid="msg_a1", pid="prt_r1", ptype="reasoning", text="thinking about it")
    usage(sid, "msg_a1")
    finish(sid)


def sc_steered(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_b1")
    emit("text", sid=sid, mid="msg_b1", pid="prt_s1", text="second turn")
    finish(sid)


def sc_tools(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("tool", sid=sid, mid="msg_a1", pid="prt_x1", call="call_1", name="bash", status="running", input={"command": "ls"})
    emit("tool", sid=sid, mid="msg_a1", pid="prt_x1", call="call_1", name="bash", status="completed", input={"command": "ls"}, output="a.txt\nb.txt")
    emit("tool", sid=sid, mid="msg_a1", pid="prt_x2", call="call_2", name="reno_report", status="running", input={"status": "done"})
    emit("tool", sid=sid, mid="msg_a1", pid="prt_x2", call="call_2", name="reno_report", status="failed" if V2 else "error", input={"status": "done"}, output="boom: no\nmore")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="done")
    finish(sid)


def sc_permission(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    props = {"id": "per_1", "sessionID": sid, "permission": "bash", "patterns": ["rm *"], "metadata": {"command": "rm -rf build"}}
    ST.pending_permissions["per_1"] = props
    emit("permission", sid=sid, props=props)
    reply = wait_reply("per_1", sid)
    ST.pending_permissions.pop("per_1", None)
    if aborted(sid):
        emit("idle", sid=sid)
        return
    word = (reply or {}).get("value")
    emit("tool", sid=sid, mid="msg_a1", pid="prt_x1", call="call_1", name="bash", status="running", input={"command": "rm -rf build"})
    if word == "once":
        emit("tool", sid=sid, mid="msg_a1", pid="prt_x1", call="call_1", name="bash", status="completed", input={"command": "rm -rf build"}, output="removed")
        emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="ran it")
    else:
        emit("tool", sid=sid, mid="msg_a1", pid="prt_x1", call="call_1", name="bash", status="failed" if V2 else "error", input={"command": "rm -rf build"}, output="The user rejected permission")
        emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="refused")
    finish(sid)


def sc_child(sid, rest):
    child = "ses_child" + uuid.uuid4().hex[:8]
    emit("busy", sid=sid)
    emit("child", sid=sid, child=child)
    emit("message", sid=sid, mid="msg_a1")
    emit("tool", sid=sid, mid="msg_a1", pid="prt_k1", call="call_k", name="task", status="running", input={"description": "dig"})
    props = {"id": "per_c", "sessionID": child, "permission": "edit", "patterns": ["a.rs"], "metadata": {"filepath": "/w/a.rs"}}
    ST.pending_permissions["per_c"] = props
    emit("permission", sid=sid, props=props)
    reply = wait_reply("per_c", sid)
    ST.pending_permissions.pop("per_c", None)
    if aborted(sid):
        emit("idle", sid=sid)
        return
    emit("tool", sid=sid, mid="msg_a1", pid="prt_k1", call="call_k", name="task", status="completed", input={"description": "dig"}, output="child said " + str((reply or {}).get("value")))
    finish(sid)


def sc_question(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    props = {"id": "que_1", "sessionID": sid, "questions": [
        {"question": "Which colour?", "header": "Colour", "options": [{"label": "red"}, {"label": "blue"}]},
        {"question": "Which sizes?", "multiple": True, "options": [{"label": "S"}, {"label": "L"}]},
    ]}
    ST.pending_questions["que_1"] = props
    emit("question", sid=sid, props=props)
    reply = wait_reply("que_1", sid)
    ST.pending_questions.pop("que_1", None)
    if aborted(sid):
        emit("idle", sid=sid)
        return
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="you said " + json.dumps((reply or {}).get("value")))
    finish(sid)


def sc_retry(sid, rest):
    emit("busy", sid=sid)
    for attempt in (1, 2, 3, 4):
        emit("retry", sid=sid, attempt=attempt, text="overloaded")
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="recovered")
    finish(sid)


def sc_retry_forever(sid, rest):
    emit("busy", sid=sid)
    for attempt in range(1, 12):
        emit("retry", sid=sid, attempt=attempt, text="provider down")
        if aborted(sid):
            break
        time.sleep(0.05)
    deadline = time.time() + 20
    while not aborted(sid) and time.time() < deadline:
        time.sleep(0.02)
    finish(sid)


def sc_error(sid, rest):
    emit("busy", sid=sid)
    emit("error", sid=sid, text="invalid api key")
    finish(sid)


def sc_error_after_text(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="partial answer")
    emit("error", sid=sid, text="late failure")
    finish(sid)


def sc_silent(sid, rest):
    # Accepted, and not one event of the session: the stall bound's case.
    deadline = time.time() + 30
    while not aborted(sid) and time.time() < deadline:
        time.sleep(0.02)
    emit("idle", sid=sid)


def sc_hang(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="working")
    deadline = time.time() + 30
    while not aborted(sid) and time.time() < deadline:
        time.sleep(0.02)
    if V2:
        emit("interrupted", sid=sid)
    else:
        emit("idle", sid=sid)


def sc_hang_no_abort_ack(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="working")
    time.sleep(60)


def sc_die(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="about to crash")
    time.sleep(0.2)
    sys.stderr.write("panic: out of memory (api_key=supersecretkey)\n")
    sys.stderr.flush()
    os._exit(3)


def sc_tool_process(sid, rest):
    # A "tool" that leaves a process behind, in its own session: the run's end must reap it.
    child = subprocess.Popen(["sleep", "300"], start_new_session=True)
    with open(os.path.join(ROOT, "tool.pid"), "w") as handle:
        handle.write(str(child.pid))
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("tool", sid=sid, mid="msg_a1", pid="prt_x1", call="call_1", name="bash", status="running", input={"command": "sleep 300"})
    deadline = time.time() + 30
    while not aborted(sid) and time.time() < deadline:
        time.sleep(0.02)
    if aborted(sid):
        emit("idle", sid=sid)
    else:
        finish(sid)


def sc_fast_fail(sid, rest):
    emit("busy", sid=sid)
    emit("error", sid=sid, text="model not found")
    finish(sid)


def sc_unknown(sid, rest):
    emit("busy", sid=sid)
    emit("unknown", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="ok")
    finish(sid)


def sc_gap(sid, rest):
    # The bus drops mid-turn: a request asked in the gap is only listed, never replayed.
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    props = {"id": "per_gap", "sessionID": sid, "permission": "bash", "patterns": ["ls"], "metadata": {"command": "ls"}}
    ST.pending_permissions["per_gap"] = props
    BUS.close_all()
    time.sleep(0.3)
    reply = wait_reply("per_gap", sid)
    ST.pending_permissions.pop("per_gap", None)
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="after the gap: " + str((reply or {}).get("value")))
    finish(sid)


def sc_gap_idle(sid, rest):
    # The bus drops and the turn ends in the gap: its idle is never seen on the bus.
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="finished in the gap")
    BUS.close_all()
    ST.running.pop(sid, None)


def sc_echo(sid, rest):
    # A prompt that names no scenario (a steering message): say it back.
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_b1")
    emit("text", sid=sid, mid="msg_b1", pid="prt_e1", text="steered:" + rest.strip())
    finish(sid)


def sc_steer(sid, rest):
    # The first turn of a steering run: it stays busy long enough for the steering message to be
    # queued, which then goes out as the next prompt.
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="working on it")
    time.sleep(1.5)
    finish(sid)


def sc_conf_happy(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    for call, name, cmd in (("c1", "bash", {"command": "echo hi"}), ("c2", "edit", {"filePath": "/w/a.rs", "oldString": "a", "newString": "b"})):
        emit("tool", sid=sid, mid="msg_a1", pid="prt_" + call, call=call, name=name, status="running", input=cmd)
        emit("tool", sid=sid, mid="msg_a1", pid="prt_" + call, call=call, name=name, status="completed", input=cmd, output="ok")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="conf-hello")
    finish(sid)


def sc_conf_question(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    props = {"id": "que_c", "sessionID": sid, "questions": [
        {"question": "Pick one", "header": "Pick", "options": [{"label": "first"}, {"label": "second"}]}]}
    ST.pending_questions["que_c"] = props
    emit("question", sid=sid, props=props)
    reply = wait_reply("que_c", sid)
    ST.pending_questions.pop("que_c", None)
    value = (reply or {}).get("value")
    label = value[0][0] if isinstance(value, list) else (value or {}).get("q0")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="answered:" + str(label))
    finish(sid)


def sc_resumed(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="back again")
    finish(sid)


def sc_mcp_config(sid, rest):
    names = [n["name"] for n in mcp_listing()["data"]] if V2 else list(mcp_listing())
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="mcp configured:" + ",".join(names))
    finish(sid)


def sc_conf_unknown(sid, rest):
    emit("busy", sid=sid)
    emit("unknown", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="unknown-done")
    finish(sid)


def sc_approval_subjects(sid, rest):
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    asks = [
        ("bash", "per_s1", {"permission": "bash", "patterns": ["rm *"], "metadata": {"command": "rm -rf build"}}),
        ("edit", "per_s2", {"permission": "edit", "patterns": ["a.rs"], "metadata": {"filepath": "/w/a.rs"}}),
        ("mcp", "per_s3", {"permission": "reno_report", "patterns": ["*"]}),
        ("net", "per_s4", {"permission": "webfetch", "patterns": ["https://example.com"]}),
        ("read", "per_s5", {"permission": "read", "patterns": ["/w/a.rs"]}),
        ("other", "per_s6", {"permission": "doom_loop", "patterns": ["*"]}),
    ]
    outcome = []
    for label, rid, props in asks:
        props = {"id": rid, "sessionID": sid, **props}
        ST.pending_permissions[rid] = props
        emit("permission", sid=sid, props=props)
        reply = wait_reply(rid, sid)
        ST.pending_permissions.pop(rid, None)
        outcome.append("%s=%s" % (label, "allow" if (reply or {}).get("value") == "once" else "deny"))
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text=" ".join(outcome))
    finish(sid)


def sc_edit_file(sid, rest):
    # An edit of a file in the working directory, asked for when the mode asks, then made.
    emit("busy", sid=sid)
    emit("message", sid=sid, mid="msg_a1")
    name = rest.split("file:")[1].split()[0]
    rules = agent_rules("build")
    ask = any(r.get("permission", r.get("action")) == "edit" and r.get("action", r.get("effect")) == "ask" for r in rules)
    allowed = True
    if ask:
        props = {"id": "per_e", "sessionID": sid, "permission": "edit", "patterns": [name], "metadata": {"filepath": os.path.join(os.getcwd(), name)}}
        ST.pending_permissions["per_e"] = props
        emit("permission", sid=sid, props=props)
        allowed = (wait_reply("per_e", sid) or {}).get("value") == "once"
        ST.pending_permissions.pop("per_e", None)
    if allowed:
        with open(os.path.join(os.getcwd(), name), "w") as handle:
            handle.write("edited\n")
    emit("text", sid=sid, mid="msg_a1", pid="prt_t1", text="edit asked" if ask else "edit unasked")
    finish(sid)


SCENARIOS = {
    "text": sc_text, "steered": sc_steered, "tools": sc_tools, "permission": sc_permission, "child": sc_child,
    "question": sc_question, "retry": sc_retry, "retry-forever": sc_retry_forever, "error": sc_error,
    "error-after-text": sc_error_after_text, "silent": sc_silent, "hang": sc_hang,
    "hang-no-abort-ack": sc_hang_no_abort_ack, "die": sc_die, "tool-process": sc_tool_process,
    "fast-fail": sc_fast_fail, "unknown": sc_unknown, "gap": sc_gap, "gap-idle": sc_gap_idle,
    "echo": sc_echo, "steer": sc_steer, "conf-happy": sc_conf_happy, "conf-question": sc_conf_question,
    "resumed": sc_resumed, "mcp-config": sc_mcp_config, "conf-unknown": sc_conf_unknown,
    "approval-subjects": sc_approval_subjects, "interrupt": sc_hang, "conf-crash": sc_die,
    "edit-file": sc_edit_file,
}


def play(sid, text):
    match = re.search(r"scenario:([a-z-]+)", text)
    name = match.group(1) if match else ("echo" if text.strip() else "text")
    ST.running[sid] = True
    ST.aborted[sid] = threading.Event()
    try:
        SCENARIOS[name](sid, text)
    finally:
        ST.running.pop(sid, None)


# ---- HTTP --------------------------------------------------------------------------------------

HTML = b"<!doctype html><html><title>OpenCode</title></html>"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    # -- helpers
    def authorized(self):
        header = self.headers.get("Authorization", "")
        if not header.startswith("Basic "):
            return False
        try:
            user, _, password = base64.b64decode(header[6:]).decode().partition(":")
        except Exception:
            return False
        return user == "opencode" and password == PASSWORD

    def reply(self, status, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def body(self):
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            return json.loads(raw) if raw else None
        except ValueError:
            return None

    def wrap(self, value):
        return {"data": value} if V2 else value

    def handle_any(self):
        url = urlparse(self.path)
        path = url.path
        query = parse_qs(url.query)
        body = self.body() if self.command == "POST" else None
        auth = self.authorized()
        log({"req": {"m": self.command, "p": path, "q": {k: v[0] for k, v in query.items()}, "auth": auth,
                     "dir": self.headers.get("x-opencode-directory"), "body": body}})
        if not auth:
            self.send_response(401)
            self.send_header("www-authenticate", 'Basic realm="Secure Area"')
            self.send_header("content-length", "0")
            self.end_headers()
            return
        parts = [p for p in path.split("/") if p]
        m = self.command
        # ---- health: the other generation's routes answer the web UI (200 HTML)
        if m == "GET" and path == "/global/health":
            return self.reply(200, HTML, "text/html") if V2 else self.reply(200, {"healthy": True, "version": VERSION})
        if m == "GET" and path == "/api/info":
            return self.reply(200, {"version": VERSION, "pid": os.getpid()}) if V2 else self.reply(200, HTML, "text/html")
        if m == "GET" and path == "/api/status":
            return self.reply(200, HTML, "text/html") if not V2 else self.reply(404, b"")
        if m == "GET" and path == "/api/health":
            return self.reply(200, {"healthy": True}) if not V2 else self.reply(404, b"")
        # ---- the bus
        if m == "GET" and path == ("/api/event" if V2 else "/global/event"):
            return self.stream()
        # ---- discovery
        if m == "GET" and path == "/provider" and not V2:
            return self.reply(200, catalog_v1())
        if m == "GET" and path == "/api/model" and V2:
            return self.reply(200, catalog_v2())
        if m == "GET" and path in ("/agent", "/api/agent") and path == ("/api/agent" if V2 else "/agent"):
            if V2:
                return self.reply(200, {"data": [{"id": a["name"], "name": a["name"].title(), "mode": a["mode"],
                                                  "hidden": a.get("hidden", False), "permissions": agent_rules(a["name"])} for a in AGENTS]})
            return self.reply(200, [{**a, "permission": agent_rules(a["name"])} for a in AGENTS])
        if m == "GET" and len(parts) == 3 and parts[:2] == ["api", "agent"] and V2:
            if os.environ.get("FAKE_OPENCODE_RULES") == "late":
                marker = os.path.join(ROOT, "rules-late")
                if not os.path.exists(marker):
                    open(marker, "w").close()
                    return self.reply(200, {"data": {"id": parts[2], "permissions": [{"action": "*", "resource": "*", "effect": "allow"}]}})
            return self.reply(200, {"data": {"id": parts[2], "name": parts[2], "permissions": agent_rules(parts[2])}})
        if m == "GET" and path == ("/api/mcp" if V2 else "/mcp"):
            return self.reply(200, mcp_listing())
        # ---- sessions
        if m == "POST" and path == ("/api/session" if V2 else "/session"):
            if V2:
                directory = ((body or {}).get("location") or {}).get("directory")
                agent = (body or {}).get("agent")
            else:
                directory = query.get("directory", [self.headers.get("x-opencode-directory")])[0]
                agent = None
            sid = new_session(directory, agent)
            return self.reply(200, self.wrap({"id": sid, "directory": directory}))
        if m == "GET" and len(parts) == 2 and parts[0] == ("api" if V2 else "session") and not V2 and parts[1] != "status":
            session = load_session(parts[1])
            return self.reply(200, session) if session else self.reply(404, {"name": "NotFoundError"})
        if m == "GET" and len(parts) == 3 and parts[:2] == ["api", "session"] and V2 and parts[2] != "active":
            session = load_session(parts[2])
            return self.reply(200, {"data": session}) if session else self.reply(404, {"_tag": "SessionNotFoundError"})
        if m == "GET" and path == ("/api/session/active" if V2 else "/session/status"):
            return self.reply(200, self.wrap({sid: {"type": "busy"} for sid in ST.running}))
        base = ["api", "session"] if V2 else ["session"]
        n = len(base)
        if len(parts) > n and parts[:n] == base:
            sid = parts[n]
            rest = parts[n + 1:]
            if m == "POST" and rest == ([ "prompt"] if V2 else ["prompt_async"]):
                return self.prompt(sid, body)
            if m == "POST" and rest == (["interrupt"] if V2 else ["abort"]):
                if sid in ST.aborted:
                    ST.aborted[sid].set()
                return self.reply(200, self.wrap(True))
            if m == "POST" and V2 and rest == ["model"]:
                return self.reply(200, {"data": {}})
            if m == "POST" and V2 and rest == ["agent"]:
                return self.reply(200, {"data": {}})
            if m == "GET" and len(rest) == 2 and rest[0] == "message":
                return self.reply(200, self.wrap({"id": rest[1]})) if rest[1] in ST.messages else self.reply(404, {"name": "NotFoundError"})
            if m == "GET" and rest == ["children"] and not V2:
                return self.reply(200, [])
            if m == "POST" and V2 and len(rest) == 3 and rest[0] == "permission" and rest[2] == "reply":
                if not body or "decision" not in body:
                    return self.reply(400, {"_tag": "InvalidRequestError", "message": 'Missing key\n  at ["decision"]'})
                return self.answer(rest[1], {"value": body["decision"]})
            if m == "POST" and V2 and len(rest) == 3 and rest[0] == "form" and rest[2] == "reply":
                if not body or "answer" not in body:
                    return self.reply(400, {"_tag": "InvalidRequestError", "message": 'Missing key\n  at ["answer"]'})
                return self.answer(rest[1], {"value": body["answer"]})
            if m == "POST" and not V2 and len(rest) == 2 and rest[0] == "permissions":
                return self.answer(rest[1], {"value": (body or {}).get("response")})
            if m == "GET" and V2 and rest == ["permission"]:
                return self.reply(200, {"data": [self.v2_permission(p) for p in ST.pending_permissions.values() if p["sessionID"] == sid or True]})
            if m == "GET" and V2 and rest == ["form"]:
                return self.reply(200, {"data": [self.v2_form(p) for p in ST.pending_questions.values()]})
        # ---- v1 request replies and lists
        if not V2:
            if m == "POST" and len(parts) == 3 and parts[0] == "permission" and parts[2] == "reply":
                if not body or "reply" not in body:
                    return self.reply(400, {"name": "BadRequest"})
                return self.answer(parts[1], {"value": body["reply"]})
            if m == "POST" and len(parts) == 3 and parts[0] == "question" and parts[2] == "reply":
                return self.answer(parts[1], {"value": (body or {}).get("answers")})
            if m == "POST" and len(parts) == 3 and parts[0] == "question" and parts[2] == "reject":
                return self.answer(parts[1], {"value": "rejected"})
            if m == "GET" and path == "/permission":
                return self.reply(200, list(ST.pending_permissions.values()))
            if m == "GET" and path == "/question":
                return self.reply(200, list(ST.pending_questions.values()))
        # Anything else: the web UI's catch-all, like the real server.
        if m == "GET":
            return self.reply(200, HTML, "text/html")
        return self.reply(404, {"error": "no such route " + path})

    def v2_permission(self, p):
        return {"id": p["id"], "sessionID": p["sessionID"], "action": p["permission"], "resources": p.get("patterns", []), "metadata": p.get("metadata", {})}

    def v2_form(self, p):
        frames = render_v2("question", {"props": p})
        return frames[0]["data"]["form"]

    def answer(self, rid, payload):
        ST.replies[rid] = payload
        ST.reply_events.setdefault(rid, threading.Event()).set()
        return self.reply(200, self.wrap(True))

    def prompt(self, sid, body):
        if os.environ.get("FAKE_OPENCODE_PROMPT") == "400":
            return self.reply(400, {"name": "BadRequest", "data": {"message": "model not found"}})
        if load_session(sid) is None:
            return self.reply(404, {"name": "NotFoundError"})
        text = ""
        if V2:
            text = (body or {}).get("text", "")
            mid = (body or {}).get("id")
        else:
            parts = (body or {}).get("parts") or [{}]
            text = parts[0].get("text", "")
            mid = (body or {}).get("messageID")
        if mid:
            ST.messages.add(mid)
        ST.prompts += 1
        log({"prompt": {"sid": sid, "text": text, "subscribers": BUS.count(), "n": ST.prompts}})
        threading.Thread(target=play, args=(sid, text), daemon=True).start()
        if os.environ.get("FAKE_OPENCODE_PROMPT") == "lose-ack" and ST.prompts == 1:
            # The server queued the prompt and only the answer is lost.
            return self.reply(500, {"name": "UnknownError"})
        return self.reply(200, {"data": {}}) if V2 else self.reply(204, b"")

    def stream(self):
        q = BUS.subscribe()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        try:
            first = frame_v2("server.connected", {}) if V2 else frame_v1("server.connected", {})
            if V2:
                first = frame_v2("server.connected", {})
            self.wfile.write(("data: " + json.dumps(first) + "\n\n").encode())
            self.wfile.flush()
            silent = os.environ.get("FAKE_OPENCODE_BUS") == "silent"
            while True:
                try:
                    frame = q.get(timeout=HEARTBEAT)
                except queue.Empty:
                    if not silent:
                        self.wfile.write(b": heartbeat\n\n")
                        self.wfile.flush()
                    continue
                if frame is None:
                    return
                if silent:
                    continue
                self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass
        finally:
            BUS.drop(q)
            self.close_connection = True

    do_GET = handle_any
    do_POST = handle_any


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False


signal.signal(signal.SIGTERM, lambda *_: os._exit(0))
server = Server((HOST, PORT), Handler)
sys.stdout.write("%sserver listening on http://%s:%d\n" % ("" if V2 else "opencode ", HOST, PORT))
sys.stdout.flush()
server.serve_forever()
