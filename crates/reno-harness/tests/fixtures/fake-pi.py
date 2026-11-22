#!/usr/bin/env python3
"""A fake `pi --mode rpc` with the shapes of Pi 1.5.0 (docs/rpc*.md, json.md), for driver tests.

The prompt of a run names the scenario it plays (`scenario:<name>`); anything else is answered
`reply:<prompt>`. Set FAKE_PI_LOG to a file and every argument vector, environment fact and
command frame received is appended to it as one JSON line, for the tests to read.

Never talks to a model, a network or a Pi settings directory.

With RENO_GATE_SOCKET set the fake plays the gate before it reads any command. FAKE_PI_GATE picks
how: `ok` (the default), `no-hello`, `never-connect`, `wrong-nonce`, `no-command` (a hello, but
`get_commands` does not list the gate) or `second-client` (a hello, then a second connection that
the daemon must not serve). The outcome of the second connection and of one made when the first
prompt arrives is logged.

Armed, a run plays Pi's tool loop through the gate: for each scripted tool it emits
`tool_execution_start`, sends the gate's call line and waits for the decision; on `allow` it runs
the tool (only `bash` has an effect: it runs the command) and logs it, on a deny it ends the tool
with an error carrying the reason. Scenarios: `gate-coverage` (one call of every kind, siblings one
at a time and a codemode call with nested ones), `gate-200` (200 reads), `gate-hold` (three calls
sent together, left open until the run is stopped; every decision is logged to the end of the
connection), `gate-late` (a second call made while the run is being stopped) and `gate-drop` (the gate's connection closes mid-run) and `gate-flood` (a broken gate that sends 17
calls without waiting). Every decision
the gate receives is logged as `gate_decision`.
"""
import json
import os
import socket
import subprocess
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


def credential_env():
    """Whether each credential variable is set and points into the agent dir, never its value."""
    agent = os.environ.get("PI_CODING_AGENT_DIR")
    facts = {}
    for name in ("GOOGLE_APPLICATION_CREDENTIALS", "AWS_SHARED_CREDENTIALS_FILE",
                 "AWS_CONFIG_FILE", "PI_CODING_AGENT_DIR"):
        value = os.environ.get(name)
        facts[name] = {"set": value is not None,
                       "in_agent_dir": bool(value and agent and value.startswith(agent + os.sep)),
                       "exists": bool(value and os.path.exists(value))}
    return facts


log({"argv": args, "cwd": os.getcwd(), "claudecode": "CLAUDECODE" in os.environ,
     "offline": os.environ.get("PI_OFFLINE"), "telemetry": os.environ.get("PI_TELEMETRY"),
     "mcp_env": os.environ.get("RENO_PI_MCP"), "leak": os.environ.get("FAKE_PI_LEAK"),
     "gate_socket": os.environ.get("RENO_GATE_SOCKET"),
     "gate_nonce_len": len(os.environ["RENO_GATE_NONCE"]) if "RENO_GATE_NONCE" in os.environ else None,
     "gate_nonce": os.environ.get("RENO_GATE_NONCE"),
     "agent_dir": os.environ.get("PI_CODING_AGENT_DIR"),
     "credential_env": credential_env(),
     "key_names": sorted(name for name in os.environ
                         if name.endswith(("_API_KEY", "_TOKEN", "_KEY", "_SECRET", "_ACCESS_KEY_ID"))
                         or name.startswith(("ANTHROPIC_", "OPENAI_", "AWS_", "GOOGLE_"))),
     "session_dir": flag("--session-dir"),
     "extensions": {path: os.path.exists(path) for flag_name, path in zip(args, args[1:])
                    if flag_name == "--extension"}})

GATE_SOCKET = os.environ.get("RENO_GATE_SOCKET")
GATE_MODE = os.environ.get("FAKE_PI_GATE", "ok") if GATE_SOCKET else None
GATE_CONNECTIONS = []
GATE_READER = None
GATE_IDS = iter(range(1, 1000000))


def gate_connect():
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(GATE_SOCKET)
    GATE_CONNECTIONS.append(client)
    return client


def gate_hello(client, nonce):
    client.sendall((json.dumps({"t": "hello", "nonce": nonce}) + "\n").encode())


def gate_outcome(client):
    """Whether the daemon closed `client` (what it does to a connection it will not serve)."""
    client.settimeout(5)
    try:
        return "served" if client.recv(1) else "closed"
    except socket.timeout:
        return "open"
    except OSError:
        return "closed"


def arm_gate():
    if GATE_MODE == "never-connect":
        return
    nonce = os.environ.get("RENO_GATE_NONCE", "")
    try:
        client = gate_connect()
    except OSError:
        log({"gate": "connect-failed"})
        return
    if GATE_MODE == "no-hello":
        client.close()
    elif GATE_MODE == "wrong-nonce":
        gate_hello(client, "not-" + nonce)
    else:
        gate_hello(client, nonce)
        global GATE_READER
        GATE_READER = client.makefile("r")
    if GATE_MODE == "second-client":
        try:
            second = gate_connect()
            gate_hello(second, nonce)
            log({"gate_second": gate_outcome(second)})
        except OSError:
            log({"gate_second": "refused"})


def note_late_gate_connection():
    """The first prompt is only sent once the gate is armed: nothing may accept by now."""
    try:
        gate_connect()
        log({"gate_late": "connected"})
    except OSError:
        log({"gate_late": "refused"})


if GATE_SOCKET:
    arm_gate()

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
        commands = []
        if GATE_SOCKET and GATE_MODE != "no-command":
            commands.append({"name": "reno-gate", "description": "Reno's gate on tool calls",
                             "source": "extension", "sourceInfo": {"path": "gate.mjs"}})
        reply(cmd, {"commands": commands})
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


def gate_send(name, args, call_id, parent=None):
    """Send one call line to the gate and return its id."""
    n = next(GATE_IDS)
    line = json.dumps({"t": "call", "id": n, "toolName": name, "toolCallId": call_id,
                       "parentToolCallId": parent, "input": args})
    GATE_CONNECTIONS[0].sendall((line + "\n").encode())
    return n


def gate_decision():
    """The next decision line from the gate's connection; None once it is closed."""
    line = GATE_READER.readline()
    if not line:
        return None
    decision = json.loads(line)
    log({"gate_decision": decision})
    return decision


def tool_start(call_id, name, args, parent=None):
    frame = {"type": "tool_execution_start", "toolCallId": call_id, "toolName": name, "args": args}
    if parent:
        frame["parentToolCallId"] = parent
    emit(frame)


def tool_end(call_id, name, text, error=False, parent=None):
    frame = {"type": "tool_execution_end", "toolCallId": call_id, "toolName": name,
             "result": {"content": [{"type": "text", "text": text}]}, "isError": error}
    if parent:
        frame["parentToolCallId"] = parent
    emit(frame)


def run_tool(name, args):
    if name == "bash":
        done = subprocess.run(["sh", "-c", args["command"]], capture_output=True, text=True)
        return done.stdout
    return "ran " + name


def gated_tool(call_id, name, args, parent=None, inside=None):
    """What Pi does for one tool: announce it, ask the gate, then run it or end it with the reason."""
    tool_start(call_id, name, args, parent)
    gate_send(name, args, call_id, parent)
    decision = gate_decision()
    if decision is None:
        sys.exit(3)
    if decision["decision"] == "allow":
        log({"ran": {"tool": name, "call_id": call_id, "input": args}})
        if inside:
            inside()
        tool_end(call_id, name, run_tool(name, args), parent=parent)
    else:
        tool_end(call_id, name, decision.get("reason") or "blocked", error=True, parent=parent)


def drain_gate():
    """Log every decision that still arrives, until the daemon closes the connection."""
    while gate_decision() is not None:
        pass


def stop_when_asked():
    """Settle as aborted when the daemon asks, then log what the gate still sends until it closes."""
    abort = wait_until(lambda c: c["type"] == "abort")
    assistant("partial", stop="aborted", usage=False)
    settle(aborted=True)
    reply(abort)
    drain_gate()
    sys.exit(0)


def play_gate(prompt, cmd):
    start(cmd)
    if prompt == "scenario:gate-coverage":
        gated_tool("g-read", "read", {"path": "a.txt"})
        gated_tool("g-grep", "grep", {"pattern": "needle", "path": "src", "glob": "*.rs"})
        gated_tool("g-find", "find", {"pattern": "*.md"})
        gated_tool("g-ls", "ls", {})
        gated_tool("g-bash", "bash", {"command": "echo hi > canary.txt"})
        gated_tool("g-edit", "edit", {"path": "a.txt", "edits": [{"oldText": "x", "newText": "y"}]})
        gated_tool("g-write", "write", {"path": "b.txt", "content": "hello"})
        gated_tool("g-mcp", "mcp__reno__ping", {"message": "hi"})
        for n in (1, 2, 3):
            gated_tool("g-sib%d" % n, "read", {"path": "sib%d.txt" % n})

        def nested():
            gated_tool("g-code/1", "mcp__reno__ping", {"message": "nested"}, parent="g-code")
            gated_tool("g-code/2", "read", {"path": "deep.txt"}, parent="g-code")

        gated_tool("g-code", "codemode", {"code": "await ping(); await read()"}, inside=nested)
        assistant("gate-covered")
    elif prompt == "scenario:gate-200":
        for n in range(200):
            gated_tool("g200-%d" % n, "read", {"path": "f%d.txt" % n})
        assistant("gate-200-done")
    elif prompt == "scenario:gate-home":
        gated_tool("g-home", "read", {"path": "~/.reno/secret"})
        assistant("gate-home-done")
    elif prompt == "scenario:gate-hold":
        for n in (1, 2, 3):
            call_id = "h-%d" % n
            tool_start(call_id, "bash", {"command": "sleep %d" % n})
            gate_send("bash", {"command": "sleep %d" % n}, call_id)
        for n in (1, 2, 3):
            decision = gate_decision()
            if decision is None:
                sys.exit(3)
            tool_end("h-%d" % n, "bash", decision.get("reason") or "ran", error=decision["decision"] != "allow")
        stop_when_asked()
    elif prompt == "scenario:gate-late":
        # The second call is made while the daemon is stopping the run: its abort is still unread.
        gated_tool("late-1", "bash", {"command": "echo one"})
        gated_tool("late-2", "bash", {"command": "echo two"})
        stop_when_asked()
    elif prompt == "scenario:gate-drop":
        GATE_READER.close()
        GATE_CONNECTIONS[0].close()
        time.sleep(30)
    elif prompt == "scenario:gate-flood":
        for n in range(17):
            tool_start("f-%d" % n, "read", {"path": "f%d.txt" % n})
            gate_send("read", {"path": "f%d.txt" % n}, "f-%d" % n)
        drain_gate()
        sys.exit(0)
    settle()


def play(cmd):
    prompt = cmd["message"]
    if GATE_READER is not None and prompt.startswith("scenario:gate-"):
        return play_gate(prompt, cmd)
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
        # Pi goes idle just as the steer arrives: the run settles first, and then Pi starts a run
        # of its own for the message. The steer is read before the settle is sent, so the driver
        # always has it in flight when the first settle reaches it.
        start(cmd)
        assistant("done-early")
        steer = wait_until(is_steer)
        settle()
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
    elif prompt == "scenario:child":
        # A tool's subprocess: it outlives Pi unless the run is torn down with its group.
        child = subprocess.Popen(["sleep", "300"])
        log({"child": child.pid})
        start(cmd)
        assistant("spawned")
        settle()
    elif prompt == "scenario:leak-stderr":
        for name, value in sorted(os.environ.items()):
            if name.endswith("_API_KEY"):
                sys.stderr.write("fake pi: rejected key %s\n" % value)
        sys.stderr.flush()
        start(cmd)
        assistant("leaked")
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
        if GATE_SOCKET:
            note_late_gate_connection()
        play(command)
        # One run per process: a settled run is followed by the daemon closing stdin.
        if command["message"] in ("scenario:handled",):
            continue
    else:
        misc(command)
