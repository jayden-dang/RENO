#!/usr/bin/env python3
"""A fake `pi --mode rpc` with the shapes of Pi 1.5.0 (docs/rpc*.md, json.md), for driver tests.

The prompt of a run names the scenario it plays (`scenario:<name>`); anything else is answered
`reply:<prompt>`. Set FAKE_PI_LOG to a file and every argument vector, environment fact and
command frame received is appended to it as one JSON line, for the tests to read.

Never talks to a model, a network or a Pi settings directory.
"""
import json
import os
import sys
import time
import uuid

LOG = os.environ.get("FAKE_PI_LOG")
SESSIONS = os.environ.get("FAKE_PI_SESSIONS", "/nonexistent-fake-pi-sessions")


def log(entry):
    if LOG:
        with open(LOG, "a") as handle:
            handle.write(json.dumps(entry) + "\n")


def emit(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


if "--version" in sys.argv:
    print(os.environ.get("FAKE_PI_VERSION", "1.5.0"))
    sys.exit(0)

args = sys.argv[1:]


def flag(name):
    return args[args.index(name) + 1] if name in args else None


log({"argv": args, "cwd": os.getcwd(), "claudecode": "CLAUDECODE" in os.environ,
     "offline": os.environ.get("PI_OFFLINE"), "telemetry": os.environ.get("PI_TELEMETRY"),
     "mcp_env": os.environ.get("RENO_PI_MCP"), "leak": os.environ.get("FAKE_PI_LEAK")})

session = flag("--session-id") or str(uuid.uuid4())
session_file = os.path.join(SESSIONS, session + ".jsonl")
resumed_with_history = flag("--session-id") is not None and os.path.exists(session_file)

MODELS = [
    {"id": "fake-1", "name": "Fake One", "provider": "fake", "reasoning": True, "contextWindow": 128000,
     "thinkingLevelMap": {"xhigh": "xhigh"}},
    {"id": "fake-2", "name": "Fake Two", "provider": "fake", "reasoning": False, "contextWindow": 64000},
]
if os.environ.get("FAKE_PI_NO_MODELS"):
    MODELS = []

state = {"model": MODELS[0] if MODELS else None, "thinking": "medium", "running": False,
         "messages": 3 if resumed_with_history else 0}


def get_state_data():
    data = {
        "thinkingLevel": state["thinking"], "isStreaming": state["running"], "isCompacting": False,
        "steeringMode": "one-at-a-time", "followUpMode": "one-at-a-time", "sessionFile": session_file,
        "sessionId": session, "autoCompactionEnabled": True, "messageCount": state["messages"],
        "pendingMessageCount": 0,
    }
    if state["model"]:
        data["model"] = state["model"]
    return data


def reply(cmd, data=None, success=True, error=None):
    frame = {"id": cmd.get("id"), "type": "response", "command": cmd["type"], "success": success}
    if data is not None:
        frame["data"] = data
    if error:
        frame["error"] = error
    emit(frame)


def read_cmd():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    cmd = json.loads(line)
    log({"cmd": cmd})
    return cmd


def misc(cmd):
    """Answer a command that is not part of a run's script."""
    kind = cmd["type"]
    if kind == "get_state":
        reply(cmd, get_state_data())
    elif kind == "get_available_models":
        reply(cmd, {"models": MODELS})
    elif kind == "set_model":
        found = [m for m in MODELS if m["id"] == cmd["modelId"] and m["provider"] == cmd["provider"]]
        if not found:
            reply(cmd, success=False, error="Model not found: %s/%s" % (cmd["provider"], cmd["modelId"]))
        else:
            state["model"] = found[0]
            reply(cmd, found[0])
    elif kind == "set_thinking_level":
        state["thinking"] = cmd["level"]
        reply(cmd)
    elif kind == "get_commands":
        reply(cmd, {"commands": []})
    elif kind == "clear_queue":
        reply(cmd, {"steering": [], "followUp": []})
    elif kind == "extension_ui_response":
        pass
    else:
        reply(cmd, success=False, error="unsupported in the fake: " + kind)


def assistant(text, stop="stop", usage=True, extra=None):
    message = {"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": stop}
    if usage:
        message["usage"] = {"input": 12, "output": 3, "cacheRead": 100, "cacheWrite": 5, "totalTokens": 120}
    if extra:
        message.update(extra)
    emit({"type": "message_start", "message": {"role": "assistant", "content": [], "stopReason": "pending"}})
    emit({"type": "message_update", "usage": {"input": 12, "output": 0},
          "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": text}})
    emit({"type": "message_end", "message": message})
    state["messages"] += 2
    os.makedirs(SESSIONS, exist_ok=True) if SESSIONS != "/nonexistent-fake-pi-sessions" else None
    if SESSIONS != "/nonexistent-fake-pi-sessions" and not os.path.exists(session_file):
        with open(session_file, "w") as handle:
            handle.write(json.dumps({"type": "session", "id": session}) + "\n")


def start(cmd, disposition="started"):
    reply(cmd, {"disposition": disposition})
    if disposition == "started":
        state["running"] = True
        emit({"type": "agent_start"})
        emit({"type": "turn_start"})
        emit({"type": "message_start", "message": {"role": "user", "content": cmd["message"]}})
        emit({"type": "message_end", "message": {"role": "user", "content": cmd["message"]}})


def settle(aborted=False):
    emit({"type": "agent_end", "messages": [], "willRetry": False})
    emit({"type": "agent_settled", "aborted": aborted})
    state["running"] = False


def tool(call_id, name, args, text, error=False):
    emit({"type": "tool_execution_start", "toolCallId": call_id, "toolName": name, "args": args})
    emit({"type": "tool_execution_update", "toolCallId": call_id, "toolName": name, "args": args,
          "partialResult": {"content": [{"type": "text", "text": text[:3]}]}})
    emit({"type": "tool_execution_end", "toolCallId": call_id, "toolName": name,
          "result": {"content": [{"type": "text", "text": text}]}, "isError": error})


def wait_until(predicate):
    """Read commands until `predicate(cmd)` is true for one; answer the others."""
    while True:
        cmd = read_cmd()
        if predicate(cmd):
            return cmd
        misc(cmd)


def is_steer(cmd):
    return cmd["type"] == "prompt" and cmd.get("streamingBehavior") == "steer"


def abort_when_asked():
    """Run until the daemon aborts: the aborted message, the settle, then the abort reply."""
    cmd = wait_until(lambda c: c["type"] == "abort")
    assistant("partial", stop="aborted", usage=False)
    settle(aborted=True)
    reply(cmd)
    sys.exit(0)


def play(cmd):
    prompt = cmd["message"]
    if prompt == "scenario:conf-happy":
        start(cmd)
        emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
        emit({"type": "message_update", "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": "thinking it over"}})
        tool("t1", "bash", {"command": "echo hi"}, "hi")
        tool("t2", "edit", {"path": "a.txt", "edits": [{"oldText": "x", "newText": "y"}]}, "edited")
        tool("t3", "read", {"path": "a.txt"}, "contents")
        assistant("conf-hello")
        settle()
    elif prompt == "scenario:conf-question":
        start(cmd)
        emit({"type": "extension_ui_request", "id": "dlg-1", "method": "select", "title": "Choose",
              "options": ["first", "second"], "timeout": 60000})
        answer = wait_until(lambda c: c["type"] == "extension_ui_response")
        value = answer.get("value", "cancelled" if answer.get("cancelled") else "?")
        assistant("answered:" + value)
        settle()
    elif prompt == "scenario:dialogs":
        start(cmd)
        emit({"type": "extension_ui_request", "id": "d-confirm", "method": "confirm", "title": "Sure?"})
        confirmed = wait_until(lambda c: c["type"] == "extension_ui_response")
        emit({"type": "extension_ui_request", "id": "d-input", "method": "input", "title": "Name?"})
        typed = wait_until(lambda c: c["type"] == "extension_ui_response")
        emit({"type": "extension_ui_request", "id": "d-note", "method": "notify", "message": "fyi"})
        assistant("confirmed:%s typed:%s" % (confirmed.get("confirmed"), typed.get("value")))
        settle()
    elif prompt == "scenario:dialog-timeout":
        start(cmd)
        emit({"type": "extension_ui_request", "id": "dlg-t", "method": "select", "title": "Quick",
              "options": ["a"], "timeout": 150})
        time.sleep(0.4)
        assistant("moved-on")
        settle()
    elif prompt == "scenario:steer":
        start(cmd)
        assistant("first")
        steer = wait_until(is_steer)
        reply(steer, {"disposition": "queued"})
        emit({"type": "queue_update", "steering": [steer["message"]], "followUp": []})
        emit({"type": "message_start", "message": {"role": "user", "content": steer["message"]}})
        assistant("steered:" + steer["message"])
        settle()
    elif prompt == "scenario:steer-twice":
        start(cmd)
        assistant("first")
        one = wait_until(is_steer)
        reply(one, {"disposition": "queued"})
        two = wait_until(is_steer)
        reply(two, {"disposition": "queued"})
        assistant("got:%s|%s" % (one["message"], two["message"]))
        settle()
    elif prompt == "scenario:steer-after-settle":
        # Pi went idle between the steer being sent and arriving: it starts a run of its own.
        start(cmd)
        assistant("done-early")
        settle()
        steer = wait_until(is_steer)
        start(steer)
        assistant("late-run:" + steer["message"])
        settle()
    elif prompt == "scenario:interrupt":
        start(cmd)
        assistant("working", stop="toolUse", usage=False)
        abort_when_asked()
    elif prompt == "scenario:conf-crash":
        start(cmd)
        assistant("about to fall over", usage=False)
        sys.stderr.write("fake pi: out of memory\n")
        sys.stderr.flush()
        os._exit(7)
    elif prompt == "scenario:conf-unknown":
        start(cmd)
        emit({"type": "agent_teleported", "where": "mars"})
        emit({"type": "message_update", "assistantMessageEvent": {"type": "audio_delta", "delta": "x"}})
        assistant("unknown-done")
        settle()
    elif prompt == "scenario:mcp-config":
        start(cmd)
        path = os.environ.get("RENO_PI_MCP")
        names = []
        if path:
            with open(path) as handle:
                names = [server["name"] for server in json.load(handle)]
        extensions = [args[i + 1] for i, a in enumerate(args) if a == "--extension"]
        assistant("mcp configured:%s ext=%d" % (",".join(names), len(extensions)))
        settle()
    elif prompt == "scenario:user-config":
        start(cmd)
        isolated = all(f in args for f in ["--no-extensions", "--no-skills", "--no-prompt-templates", "--no-mcp"])
        assistant("user-config:isolated" if isolated else "user-config:inherited")
        settle()
    elif prompt == "scenario:resumed":
        start(cmd)
        assistant("back again")
        settle()
    elif prompt == "scenario:error":
        start(cmd)
        assistant("", stop="error", usage=False, extra={"errorMessage": "provider said no"})
        settle()
    elif prompt == "scenario:retry":
        start(cmd)
        assistant("", stop="error", usage=False, extra={"errorMessage": "overloaded"})
        emit({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 1, "errorMessage": "overloaded"})
        emit({"type": "auto_retry_end", "success": True, "attempt": 2})
        assistant("recovered")
        settle()
    elif prompt == "scenario:length":
        start(cmd)
        assistant("cut off", stop="length")
        settle()
    elif prompt == "scenario:handled":
        start(cmd, disposition="handled")
        # An extension command ran: no run, nothing to wait for.
    elif prompt == "scenario:handled-then-run":
        # The handler began work of its own: the state barrier must see it.
        start(cmd, disposition="handled")
        state["running"] = True
        emit({"type": "agent_start"})
        probe = wait_until(lambda c: c["type"] == "get_state")
        reply(probe, get_state_data())
        assistant("extension-run")
        settle()
    elif prompt == "scenario:reject":
        reply(cmd, success=False, error="No API key found for fake")
    elif prompt == "scenario:wedged":
        start(cmd)
        time.sleep(60)
    elif prompt == "scenario:which-model":
        start(cmd)
        assistant("%s/%s/%s" % (state["model"]["provider"], state["model"]["id"], state["thinking"]))
        settle()
    elif prompt == "scenario:env":
        start(cmd)
        assistant("env:%s" % ("CLAUDECODE" in os.environ))
        settle()
    else:
        start(cmd)
        assistant("reply:" + prompt)
        settle()


while True:
    command = read_cmd()
    if command["type"] == "prompt":
        play(command)
        # One run per process: a settled run is followed by the daemon closing stdin.
        if command["message"] in ("scenario:handled",):
            continue
    else:
        misc(command)
