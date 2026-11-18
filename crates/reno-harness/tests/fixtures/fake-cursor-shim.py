#!/usr/bin/env python3
"""A fake Cursor shim for driver tests: speaks the JSONL protocol of
`src/drivers/cursor/shim.mjs` (the one Reno materializes) without node or the SDK.

The prompt of a run names the scenario it plays (`scenario:<name>`); anything else is answered
`reply:<prompt>`. Set FAKE_CURSOR_LOG to a file and every argument vector, environment fact and
frame received is appended to it as one JSON line, for the tests to read.

`models` mode answers with the shapes of 1.0.28's `Cursor.models.list()`. FAKE_CURSOR_MODELS
picks another answer: `auth` (a fatal that says the key is refused), `net` (a fatal that is not
about the key), `none` (no catalog frame).

Deterministic: every frame is a reaction to a frame it was sent, never to a timer. Never talks to
a model, a network, a browser or the owner's Cursor directory.
"""
import json
import os
import re
import signal
import subprocess
import sys
import time
import uuid

LOG = os.environ.get("FAKE_CURSOR_LOG")


def log(entry):
    if LOG:
        with open(LOG, "a") as handle:
            handle.write(json.dumps(entry) + "\n")


def emit(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def read_cmd():
    line = sys.stdin.readline()
    if not line:
        return None
    command = json.loads(line)
    log({"cmd": command})
    return command


def wait_for(op):
    """Read frames until one with `op` arrives; None at EOF."""
    while True:
        command = read_cmd()
        if command is None or command.get("op") == op:
            return command


log({
    "argv": sys.argv[1:],
    "cwd": os.getcwd(),
    "no_open_browser": os.environ.get("NO_OPEN_BROWSER"),
    "state_dir": os.environ.get("RENO_CURSOR_STATE_DIR"),
    "api_key": "CURSOR_API_KEY" in os.environ,
    "claudecode": "CLAUDECODE" in os.environ,
    "leak": os.environ.get("FAKE_CURSOR_LEAK"),
})

CATALOG = [
    {"id": "auto-smart", "displayName": "Auto",
     "parameters": [{"id": "optimize_for", "displayName": "Optimize For", "values": [
         {"value": "intelligence", "displayName": "Intelligence"},
         {"value": "balanced", "displayName": "Balance"},
         {"value": "cost", "displayName": "Cost"}]}],
     "variants": [{"params": [{"id": "optimize_for", "value": "balanced"}], "isDefault": True}]},
    {"id": "default", "displayName": "Auto", "aliases": ["auto"]},
    {"id": "claude-fable-5", "displayName": "Claude Fable 5", "description": "Anthropic frontier",
     "parameters": [{"id": "thinking", "values": [{"value": "enabled"}, {"value": "disabled"}]}]},
    {"id": "grok-4.7", "displayName": "Grok 4.7",
     "parameters": [{"id": "context", "values": [{"value": "256k"}, {"value": "500k"}]}],
     "variants": [{"params": [{"id": "context", "value": "500k"}], "isDefault": True,
                   "isMaxMode": True}]},
]

if len(sys.argv) > 1 and sys.argv[1] == "models":
    mode = os.environ.get("FAKE_CURSOR_MODELS")
    if mode == "auth":
        emit({"ev": "fatal", "message": "cursor model discovery failed: [unknown] Invalid User API Key"})
        sys.exit(1)
    if mode == "net":
        emit({"ev": "fatal", "message": "cursor model discovery failed: fetch failed (ECONNRESET)"})
        sys.exit(1)
    if mode != "none":
        emit({"ev": "models", "items": CATALOG})
    sys.exit(0)

def read_first():
    """The first frame, byte by byte: a scenario that hands stdin to another program must not
    leave the frames after it in this process's buffer."""
    line = b""
    while not line.endswith(b"\n"):
        byte = os.read(0, 1)
        if not byte:
            return None
        line += byte
    command = json.loads(line)
    log({"cmd": command})
    return command


first = read_first()
if first is None or first.get("op") != "run":
    emit({"ev": "fatal", "message": "expected op run first"})
    sys.exit(1)

prompt = first.get("prompt", "")
found = re.search(r"scenario:([a-z0-9-]+)", prompt)
scenario = found.group(1) if found else None
state_dir = os.environ.get("RENO_CURSOR_STATE_DIR")
agent = first.get("resume") or "agent-" + str(uuid.uuid4())
MODEL = first.get("model") or "composer-2.5"


def remember():
    """The shim's marker index: agent id to the store directory, as the real one writes it."""
    if not state_dir or not first.get("storeDir"):
        return
    os.makedirs(os.path.join(state_dir, "by-agent"), exist_ok=True)
    with open(os.path.join(state_dir, "by-agent", agent), "w") as handle:
        handle.write(first["storeDir"])


def ready():
    remember()
    emit({"ev": "ready", "agentId": agent, "model": MODEL})


def finish():
    emit({"ev": "turn", "status": "finished"})


def tool(call_id, name, args, error=False):
    emit({"ev": "tool", "phase": "start", "id": call_id, "name": name, "args": args})
    emit({"ev": "tool", "phase": "end", "id": call_id, "name": name, "args": args, "error": error})


def play():
    if scenario == "conf-happy":
        ready()
        emit({"ev": "thinking", "text": "thinking it over"})
        emit({"ev": "text", "text": "looking around. "})
        tool("c1", "shell", {"command": "ls"})
        tool("c2", "edit", {"path": "a.txt"})
        tool("c3", "read", {"path": "a.txt"})
        emit({"ev": "text", "text": "conf-hello"})
        emit({"ev": "usage", "input": 117, "output": 3, "cacheRead": 100, "cacheWrite": 5})
        finish()
    elif scenario == "subagent":
        ready()
        emit({"ev": "text", "text": "spawning. "})
        emit({"ev": "tool", "phase": "start", "id": "task1", "name": "task",
              "args": {"description": "scan repo", "prompt": "scan the repo"}})
        emit({"ev": "text", "text": "sub scanning", "parent": "task1"})
        emit({"ev": "tool", "phase": "start", "id": "s1", "name": "grep",
              "args": {"pattern": "todo"}, "parent": "task1"})
        emit({"ev": "tool", "phase": "end", "id": "s1", "name": "grep", "args": {},
              "error": False, "parent": "task1"})
        emit({"ev": "tool", "phase": "end", "id": "task1", "name": "task", "args": {},
              "error": False})
        emit({"ev": "text", "text": "main answer"})
        finish()
    elif scenario == "steer":
        ready()
        emit({"ev": "text", "text": "working"})
        command = wait_for("steer")
        if command:
            emit({"ev": "steered"})
            emit({"ev": "text", "text": "steered:" + command["prompt"]})
            finish()
    elif scenario == "steer-twice":
        ready()
        emit({"ev": "text", "text": "working"})
        got = []
        while len(got) < 2:
            command = wait_for("steer")
            if command is None:
                return
            got.append(command["prompt"])
        emit({"ev": "steered"})
        emit({"ev": "steered"})
        emit({"ev": "text", "text": "got:" + "|".join(got)})
        finish()
    elif scenario == "steer-race":
        # The turn ends before the SDK consumed the steering: the shim reports the turn, then
        # runs the input as a follow-up turn of its own. The run is not over at the first turn.
        ready()
        emit({"ev": "text", "text": "first"})
        command = wait_for("steer")
        if command:
            finish()
            emit({"ev": "steered"})
            emit({"ev": "text", "text": "late-run:" + command["prompt"]})
            finish()
    elif scenario == "interrupt":
        ready()
        emit({"ev": "text", "text": "working"})
        if wait_for("interrupt"):
            emit({"ev": "turn", "status": "cancelled"})
    elif scenario == "interrupt-fatal":
        ready()
        emit({"ev": "text", "text": "working"})
        if wait_for("interrupt"):
            emit({"ev": "fatal", "message": "transport closed during cancellation"})
            sys.exit(1)
    elif scenario == "hung-cancel":
        ready()
        emit({"ev": "text", "text": "working"})
        if wait_for("interrupt"):
            # The SDK's cancel wedged: nothing more is said, EOF is not read, and SIGTERM is
            # ignored, so only SIGKILL ends it.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            while True:
                time.sleep(3600)
    elif scenario in ("conf-crash", "crash"):
        ready()
        emit({"ev": "text", "text": "about to die"})
        sys.stderr.write("shim exploded\n")
        sys.stderr.flush()
        sys.exit(3)
    elif scenario == "fatal":
        emit({"ev": "fatal", "message": "Cursor is not connected: set CURSOR_API_KEY"})
        sys.exit(1)
    elif scenario == "leaky":
        ready()
        emit({"ev": "turn", "status": "error",
              "error": "request failed: Bearer secret-token-123 was refused"})
    elif scenario == "turn-error":
        ready()
        emit({"ev": "text", "text": "partial"})
        emit({"ev": "turn", "status": "error", "error": "model exploded"})
    elif scenario == "resumed":
        ready()
        emit({"ev": "text", "text": "back again"})
        finish()
    elif scenario == "mcp-config":
        ready()
        names = ",".join(server["name"] for server in first.get("mcpServers", []))
        emit({"ev": "text", "text": "mcp configured:" + names})
        finish()
    elif scenario == "conf-unknown":
        ready()
        emit({"ev": "hologram", "x": 1})
        tool("u1", "teleport", {})
        emit({"ev": "text", "text": "unknown-done"})
        finish()
    elif scenario == "user-config":
        ready()
        emit({"ev": "text", "text": "user-config:" + (
            "inherited" if first.get("inheritUserConfig") else "isolated")})
        finish()
    elif scenario == "child":
        # A tool's subprocess: it outlives the shim unless the run is torn down with its group.
        child = subprocess.Popen(["sleep", "300"], stdout=subprocess.DEVNULL)
        log({"child": child.pid})
        ready()
        emit({"ev": "text", "text": "spawned"})
        finish()
    elif scenario in ("burst", "burst-cancel"):
        # Another program plays the rest: it rejects overlapping SDK turns and echoes the
        # exact prompt order.
        peer = os.path.join(os.path.dirname(os.path.abspath(__file__)), "cursor-steering-peer.mjs")
        os.execvp("node", ["node", peer, json.dumps(first)])
    elif scenario == "orphan-holds-stdout":
        # The shim dies; a descendant keeps its stdout open, so EOF never comes.
        subprocess.Popen(["sleep", "300"])
        ready()
        emit({"ev": "text", "text": "then gone"})
        sys.stdout.flush()
        os._exit(0)
    else:
        ready()
        emit({"ev": "text", "text": "reply:" + prompt})
        emit({"ev": "usage", "input": 10, "output": 2, "cacheRead": 0, "cacheWrite": 0})
        finish()


play()
# One run per process: a settled run is followed by the driver closing stdin.
while read_cmd() is not None:
    pass
