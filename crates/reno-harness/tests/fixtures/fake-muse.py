#!/usr/bin/env python3
"""A stand-in `muse serve` for the Muse driver's tests: no Muse, no model, no network, no Muse
config or credentials anywhere.

It speaks MSP (JSON-RPC 2.0, one object per line) with the frame shapes of Muse Code 1.4.23, and
plays the scenario the first prompt names (`scenario:<name>`). WHERE THE SHAPES COME FROM:

- the handshake, `session/start`, `session/resume`, `session/setApprovalMode`, the whole
  `echo` turn (`echo-1.4.23.ndjson`) and the account and model answers were OBSERVED on a real
  Muse Code 1.4.23 driven over its own `echo` provider in a throwaway HOME;
- approvals, questions, tool calls, subagents and the turn-failure terminal are NOT observed on
  1.4.23 (they need a model that calls tools): their shapes follow zeron's recordings of Muse
  1.4.3 and the 1.4.23 schema (`muse schema generate-json-schema --experimental`).

Environment (all optional):
  FAKE_MUSE_LOG       a file receiving one JSON line per fact: {"argv", "env"} at launch, then
                      {"frame": <every frame the client sent>}
  FAKE_MUSE_VERSION   the CLI version `initialize` reports (default 1.4.23)
  FAKE_MUSE_ACCOUNT   `account/read`'s result as JSON; `out` is logged-out (default: signed in)
  FAKE_MUSE_NO_MCP    do not grant `sessionMcp`
  FAKE_MUSE_POSTURE   `profile` or `mode`: report the wrong permission profile / approval mode
  FAKE_MUSE_GONE      a session id `session/resume` does not know
  FAKE_MUSE_PIDFILE   where the `tool-descendant` scenario writes the pid of the process it left
  FAKE_MUSE_IGNORE_INTERRUPT   never answer `turn/interrupt`

The fake refuses to go on (exit 99, a line on stderr, and a log entry) when the client sends a
method that writes Muse's configuration: a run that did would fail loudly even where a test
forgot to look.
"""
import json
import os
import subprocess
import sys
import time
import uuid

args = sys.argv[1:]
LOG = os.environ.get("FAKE_MUSE_LOG")

# Methods of the 1.4.23 schema that write what the owner configured in Muse, or reach out as them.
WRITES = {
    "account/loginStart", "account/loginCancel", "account/logout", "settings/writeMany",
    "settings/writeValue", "mcpServer/oauthLogin", "mcpServer/oauthLoginCancel",
    "mcpServer/oauthLogout", "skill/setActivation", "skill/import", "plugin/install",
    "plugin/marketplaceAdd", "plugin/marketplaceRemove", "plugin/marketplaceUpdate",
    "computerUseSettings/update", "computerUseSetup/start", "feedback/submit", "usage/refresh",
    "session/setModel", "session/setPermissionProfile", "session/setReasoningEffort",
    "session/rename", "host/shutdown",
}


def record(entry):
    if LOG:
        with open(LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")


if args[:1] == ["--version"]:
    print("Muse Code 1.4.23 (1.4.23-R8977.1)")
    sys.exit(0)

record({"argv": args, "env": dict(os.environ)})

SESSION = None
ROOT = None
PROVIDER = args[args.index("--provider") + 1] if "--provider" in args else "meta"
TURNS = 0
SEEN = []          # frames read while a scenario waited for another one
EXPERIMENTAL = False


def write(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def read_frame():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        frame = json.loads(line)
        record({"frame": frame})
        method = frame.get("method")
        if method in WRITES:
            sys.stderr.write("fake-muse: the client sent %s, which writes Muse's config\n" % method)
            sys.stderr.flush()
            record({"refused": method})
            sys.exit(99)
        return frame


def next_frame():
    if SEEN:
        return SEEN.pop(0)
    return read_frame()


def wait_for(method, timeout=15.0):
    """Block for the next frame that is `method` (a response when `method` is "@reply"); frames
    that arrive meanwhile are kept for the main loop."""
    kept = []
    end = time.time() + timeout
    while time.time() < end:
        frame = read_frame()
        if frame is None:
            sys.exit(0)
        if method == "@reply" and "method" not in frame or frame.get("method") == method:
            SEEN[:0] = kept
            return frame
        kept.append(frame)
    SEEN[:0] = kept
    sys.stderr.write("fake-muse: timed out waiting for %s\n" % method)
    sys.exit(98)


def reply(frame, result):
    write({"jsonrpc": "2.0", "id": frame["id"], "result": result})


def fail(frame, kind, message, code=-32020):
    write({"jsonrpc": "2.0", "id": frame["id"], "error": {
        "code": code, "message": message, "data": {"kind": kind, "retryable": False}}})


def note(method, **params):
    params.setdefault("sessionId", SESSION)
    params.setdefault("viewCursor", "v10:%s:%d" % (SESSION, int(time.time() * 1000) % 100000))
    write({"jsonrpc": "2.0", "method": method, "params": params})


def uid():
    return str(uuid.uuid4())


# ---------------------------------------------------------------------------------------------
# The host
# ---------------------------------------------------------------------------------------------

def session_obj(session_id, params, source="startup", mode=None):
    wrong_profile = os.environ.get("FAKE_MUSE_POSTURE") == "profile"
    wrong_mode = os.environ.get("FAKE_MUSE_POSTURE") == "mode"
    profile = ":unrestricted" if wrong_profile else params.get("permissionProfile", ":ask-me")
    mode = mode or ("allowAll" if wrong_mode else params.get("approvalMode", "promptUnmatched"))
    return {
        "sessionId": session_id, "status": "idle", "activeTurnId": None,
        "workspaceRoot": params.get("workspaceRoot", ROOT), "providerId": PROVIDER,
        "modelId": params.get("modelId"), "turnCount": 0, "kind": "root",
        "approvalMode": {"mode": mode, "source": source, "lastCommandId": None},
        "permissionState": {"profileId": profile, "reviewerAuthority": "human",
                            "axes": {"filesystem": "sandboxed", "network": "off",
                                     "approval": "onRequest", "reviewer": "human"},
                            "reviewerAttached": True},
    }


def handle(frame):
    global SESSION, ROOT, EXPERIMENTAL
    method = frame.get("method")
    params = frame.get("params") or {}
    if method == "initialize":
        EXPERIMENTAL = bool(params.get("capabilities", {}).get("experimentalApi"))
        wants = params.get("capabilities", {}).get("requestedCapabilities", [])
        granted = [] if os.environ.get("FAKE_MUSE_NO_MCP") else [c for c in wants if c == "sessionMcp"]
        reply(frame, {
            "serverInfo": {"name": "muse", "version": os.environ.get("FAKE_MUSE_VERSION", "1.4.23")},
            "userAgent": "muse-build/1.4.23 (serve; fake)", "museHome": "/nonexistent/muse",
            "platformFamily": "unix", "platformOs": "macos",
            "schema": {"version": int(os.environ.get("FAKE_MUSE_SCHEMA", "1")),
                       "fingerprint": "sha256:fake"},
            "grantedCapabilities": granted, "experimentalApi": EXPERIMENTAL,
            "sessionDurability": "ephemeral" if "--no-session-log" in args else "durable",
            "sandboxAvailability": "available",
        })
    elif method == "initialized":
        pass
    elif method == "account/read":
        if not EXPERIMENTAL:
            fail(frame, "invalidParams", "account/read needs experimentalApi", -32602)
            return
        raw = os.environ.get("FAKE_MUSE_ACCOUNT")
        if raw == "out":
            reply(frame, {"state": "loggedOut", "credentialRequired": True})
        elif raw:
            reply(frame, json.loads(raw))
        else:
            reply(frame, {"state": "accountLogin", "label": "fake@example.com",
                          "credentialRequired": True})
    elif method == "model/list":
        beside = os.path.join(os.path.dirname(os.path.abspath(__file__)), "muse", "models.json")
        reply(frame, json.load(open(beside)))
    elif method == "session/start":
        SESSION = params["sessionId"]
        ROOT = params.get("workspaceRoot")
        info = session_obj(SESSION, params)
        note("session/started", session=info)
        reply(frame, {"session": info, "viewCursor": "v10:%s:1" % SESSION})
    elif method == "session/resume":
        gone = os.environ.get("FAKE_MUSE_GONE")
        if params["sessionId"] == gone:
            fail(frame, "sessionNotFound", "session %s not found" % gone)
            return
        SESSION = params["sessionId"]
        ROOT = params.get("workspaceRoot", ROOT)
        # The real host reports the mode the session last had: allowAll here, on purpose.
        info = session_obj(SESSION, params, source="replay", mode="allowAll")
        reply(frame, {"session": info, "viewCursor": "v10:%s:5" % SESSION,
                      "history": {"mode": "none", "noneReason": "excluded"}, "pendingRequests": []})
    elif method == "session/setApprovalMode":
        mode = "allowAll" if os.environ.get("FAKE_MUSE_POSTURE") == "mode" else params["mode"]
        reply(frame, {"commandId": params["commandId"], "status": "accepted",
                      "applyOutcome": "completed",
                      "effectiveMode": {"mode": mode, "source": "approvalReconfigure"}})
    elif method == "turn/start":
        start_turn(frame, params)
    elif method == "turn/interrupt":
        reply(frame, {"commandId": params["commandId"], "status": "accepted"})
    elif "id" in frame and method:
        fail(frame, "methodNotFound", "method not found: " + method, -32601)


# ---------------------------------------------------------------------------------------------
# Items and turns
# ---------------------------------------------------------------------------------------------

def prompt_of(params):
    return "".join(p.get("text", "") for p in params.get("input", []) if p.get("type") == "text")


def ack(frame, params, turn, disposition="started"):
    reply(frame, {"commandId": params["commandId"], "status": "accepted", "turnId": turn,
                  "startedNewTurn": disposition == "started", "disposition": disposition})


def item(kind, item_id, turn, revision, status, **fields):
    body = {"itemId": item_id, "kind": kind, "turnId": turn, "revision": revision, "status": status}
    body.update(fields)
    return body


def emit_item(event, kind, item_id, turn, revision, status, **fields):
    note("item/" + event, item=item(kind, item_id, turn, revision, status, **fields))


def user_message(turn, text):
    emit_item("completed", "userMessage", uid(), turn, 1, "completed", text=text, commandId=turn)


def begin_turn(turn, text):
    note("session/statusChanged", status="running")
    note("turn/started", turnId=turn, commandId=turn)
    user_message(turn, text)


def say(turn, text, chunks=2, item_id=None):
    """An assistant message streamed in `chunks` deltas, then completed whole."""
    item_id = item_id or uid()
    emit_item("started", "agentMessage", item_id, turn, 1, "inProgress", text="",
              messagePhase="finalAnswer")
    step = max(1, len(text) // chunks)
    for start in range(0, len(text), step):
        note("item/delta", itemId=item_id, field="text", delta=text[start:start + step])
    emit_item("completed", "agentMessage", item_id, turn, 2, "completed", text=text,
              messagePhase="finalAnswer")
    return item_id


def think(turn, text):
    item_id = uid()
    emit_item("started", "reasoning", item_id, turn, 1, "inProgress", summary=[])
    note("item/delta", itemId=item_id, field="summary.0", delta=text)
    emit_item("completed", "reasoning", item_id, turn, 2, "completed", summary=[text])


def tool(turn, call_id, name, args, output="", status="completed", **extra):
    emit_item("started", "toolCall", call_id, turn, 1, "inProgress", tool=name,
              callId="call_" + call_id, args=json.dumps(args))
    fields = {"tool": name, "callId": "call_" + call_id, "args": json.dumps(args),
              "visibleOutput": output}
    fields.update(extra)
    emit_item("completed", "toolCall", call_id, turn, 2, status, **fields)


def usage(turn, prompt=1000, out=40, cached=800):
    note("session/tokenUsage", turnId=turn, modelId=None, promptTokens=prompt, totalTokens=prompt + out,
         usage={"inputTokens": prompt, "outputTokens": out, "cachedTokens": cached,
                "cacheReadTokens": cached, "cacheWriteTokens": 0, "reasoningTokens": 0},
         cumulative={"promptTokens": prompt, "outputTokens": out, "totalTokens": prompt + out})
    note("session/contextUsage", usedTokens=prompt + out, windowTokens=128000, pressure="normal")


def finish(turn, terminal="completed", **extra):
    note("turn/foregroundCompleted", turnId=turn, blockingAgents=[])
    note("session/statusChanged", status="idle")
    note("turn/completed", turnId=turn, terminal=terminal, durationMs=12, **extra)


def requirement(approval, index):
    return {"approvalId": approval, "sourceIndex": index}


CHOICES = [
    {"choiceId": "allow_once", "label": "Allow once", "decision": "approved", "scope": "once"},
    {"choiceId": "allow_session", "label": "Allow for the session",
     "decision": "approvedForSession", "scope": "session"},
    {"choiceId": "allow_local_prefix", "label": "Always allow in this workspace",
     "decision": "approvedPolicyAmendment", "scope": "localPersistent",
     "rulePreview": "Always allow in this workspace"},
    {"choiceId": "abort", "label": "Reject", "decision": "abort", "scope": "once",
     "acceptsFeedback": True},
]


def stage(approval, index, total, argv, resolution="unresolved"):
    return {"requirementId": requirement(approval, index), "position": index + 1,
            "totalStages": total, "argv": argv, "argvComplete": True,
            "resolution": {"kind": resolution}}


def ask_approval(turn, tool_name, subject, index=0, choices=None, approval=None, stages=None):
    """Raise an approval (the request and its notification, as the host does) and return the
    `approval/decide` the client answers with."""
    approval = approval or uid()
    payload = {
        "sessionId": SESSION, "approvalId": approval, "turnId": turn, "toolName": tool_name,
        "subject": subject, "currentRequirementId": requirement(approval, index),
        "availableChoices": choices if choices is not None else CHOICES,
        "protectedWrite": False, "judgeEscalated": False,
    }
    write({"jsonrpc": "2.0", "id": 9000 + index, "method": "approval/request", "params": payload})
    note("approval/requested", **{k: v for k, v in payload.items() if k != "sessionId"})
    decide = wait_for("approval/decide")
    reply(decide, {"commandId": decide["params"]["commandId"], "status": "accepted"})
    return approval, decide["params"]


def settle(approval, turn, params, stages=None):
    chosen = next((c for c in CHOICES if c["choiceId"] == params["choiceId"]), {})
    note("approval/resolved", approvalId=approval, turnId=turn, decision=chosen.get("decision"),
         resolvedBy="user")


def shell_subject(command, stages=None):
    return {"kind": "shell", "command": command, "workspaceRoot": ROOT, "stages": stages or []}


# ---------------------------------------------------------------------------------------------
# Scenarios. Each is given the turn id and the prompt, and ends the turn itself.
# ---------------------------------------------------------------------------------------------

def scenario_echo(turn, prompt):
    """The turn a real 1.4.23 played over its `echo` provider, frames as recorded."""
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "muse", "echo-1.4.23.ndjson")
    for line in open(path):
        text = line.replace("$SESSION", SESSION).replace("$TURN", turn)
        write(json.loads(text))


def scenario_conf_happy(turn, prompt):
    begin_turn(turn, prompt)
    think(turn, "thinking it over")
    say(turn, "conf-hello")
    tool(turn, "t1", "bash", {"command": "echo hi"}, "hi")
    tool(turn, "t2", "edit", {"file_path": "a.txt", "old_string": "x", "new_string": "y"}, "edited")
    tool(turn, "t3", "read", {"path": "a.txt"}, "y")
    usage(turn, prompt=1000, out=40, cached=800)
    finish(turn)


def scenario_child(turn, prompt):
    """What the lifecycle runs: text and tools, plus a descendant of the tool left running."""
    begin_turn(turn, prompt)
    say(turn, "child started")
    path = os.environ.get("FAKE_MUSE_PIDFILE")
    child = subprocess.Popen(["sleep", "300"], start_new_session=False)
    if path:
        with open(path, "w") as f:
            f.write(str(child.pid))
    tool(turn, "t1", "bash", {"command": "sleep 300 &"}, "")
    say(turn, "child left running")
    finish(turn)


def scenario_approval(turn, prompt):
    begin_turn(turn, prompt)
    verdicts = []
    cases = [
        ("bash", "bash", shell_subject("git status"), "bash"),
        ("edit", "write", {"kind": "fileAccess", "path": "src/lib.rs", "access": "write"}, "edit"),
        ("mcp", "linear__search", {"kind": "tool"}, "mcp"),
        ("net", "web_fetch", {"kind": "network", "host": "example.com", "port": 443}, "net"),
        ("read", "read", {"kind": "fileAccess", "path": "/etc/hosts", "access": "read"}, "read"),
        ("other", "mystery", {"kind": "computerUseTakeover"}, "other"),
    ]
    for label, name, subject, _ in cases:
        approval, decide = ask_approval(turn, name, subject)
        settle(approval, turn, decide)
        chosen = decide["choiceId"]
        if chosen == "allow_once":
            verdicts.append("%s=allow" % label)
        else:
            verdicts.append("%s=deny:%s" % (label, decide.get("feedback") or "(none)"))
    say(turn, " ".join(verdicts))
    usage(turn)
    finish(turn)


def scenario_conf_approval(turn, prompt):
    """The conformance approval case: a bash command, then an edit."""
    begin_turn(turn, prompt)
    approval, bash = ask_approval(turn, "bash", shell_subject("git status"))
    settle(approval, turn, bash)
    approval, edit = ask_approval(turn, "write", {"kind": "fileAccess", "path": "a.txt", "access": "write"})
    settle(approval, turn, edit)

    def verdict(label, decide):
        if decide["choiceId"] == "allow_once":
            return "%s=allow" % label
        return "%s=deny:%s" % (label, decide.get("feedback") or "The user did not allow this.")

    say(turn, "%s %s" % (verdict("bash", bash), verdict("edit", edit)))
    finish(turn)


def scenario_staged(turn, prompt):
    begin_turn(turn, prompt)
    approval = uid()
    command = "mkdir -p out && touch out/x"
    stages = [stage(approval, 0, 2, ["mkdir", "-p", "out"]), stage(approval, 1, 2, ["touch", "out/x"])]
    subject = shell_subject(command, stages)
    _, first = ask_approval(turn, "bash", subject, 0, approval=approval)
    # The second stage: announced by approval/updated with the first resolved.
    stages = [stage(approval, 0, 2, ["mkdir", "-p", "out"], "allowOnce"), stage(approval, 1, 2, ["touch", "out/x"])]
    subject = shell_subject(command, stages)
    payload = {"sessionId": SESSION, "approvalId": approval, "currentRequirementId": requirement(approval, 1),
               "subject": subject, "availableChoices": CHOICES}
    note("approval/updated", **{k: v for k, v in payload.items() if k != "sessionId"})
    decide = wait_for("approval/decide")
    reply(decide, {"commandId": decide["params"]["commandId"], "status": "accepted"})
    second = decide["params"]
    note("approval/resolved", approvalId=approval, turnId=turn, decision="approved", resolvedBy="user")
    say(turn, "stages: %s/%s %s/%s" % (first["requirementId"]["sourceIndex"], first["choiceId"],
                                         second["requirementId"]["sourceIndex"], second["choiceId"]))
    finish(turn)


def scenario_only_broad(turn, prompt):
    """An approval whose only allows would grant more than this once (and no refusal): the run
    must take none of them."""
    begin_turn(turn, prompt)
    broad = [c for c in CHOICES if c["scope"] != "once"]
    approval = uid()
    payload = {"sessionId": SESSION, "approvalId": approval, "turnId": turn, "toolName": "bash",
               "subject": shell_subject("rm -rf build"), "currentRequirementId": requirement(approval, 0),
               "availableChoices": broad}
    note("approval/requested", **{k: v for k, v in payload.items() if k != "sessionId"})
    frame = wait_for("approval/decide", timeout=8)
    say(turn, "decided:%s" % frame["params"]["choiceId"])
    finish(turn)


def scenario_question(turn, prompt):
    begin_turn(turn, prompt)
    qid = uid()
    questions = [{"id": "q1", "header": "Scope", "question": "Which files?",
                  "options": [{"label": "All"}, {"label": "Changed"}], "selection": {"mode": "single"}}]
    params = {"sessionId": SESSION, "userInputId": qid, "turnId": turn, "itemId": "tool-ask",
              "questions": questions}
    write({"jsonrpc": "2.0", "id": 8000, "method": "userInput/request", "params": params})
    note("userInput/requested", **{k: v for k, v in params.items() if k != "sessionId"})
    frame = wait_for_any(("userInput/answer", "userInput/cancel"))
    reply(frame, {"commandId": frame["params"]["commandId"], "status": "accepted"})
    note("userInput/settled", userInputId=qid)
    if frame["method"] == "userInput/cancel":
        say(turn, "dismissed:%s" % frame["params"].get("reason", ""))
    else:
        answer = frame["params"]["answers"][0]
        say(turn, "answered:%s" % (answer.get("selectedLabel") or answer.get("freeText")))
    finish(turn)


def wait_for_any(methods, timeout=15.0):
    kept = []
    end = time.time() + timeout
    while time.time() < end:
        frame = read_frame()
        if frame is None:
            sys.exit(0)
        if frame.get("method") in methods:
            SEEN[:0] = kept
            return frame
        kept.append(frame)
    sys.exit(98)


def scenario_steer(turn, prompt):
    """Text, then a wait for a steering message: `turn/start` with ifBusy steer."""
    begin_turn(turn, prompt)
    say(turn, "working")
    frame = wait_for("turn/start")
    params = frame["params"]
    ack(frame, params, turn, "steered")
    user_message(turn, prompt_of(params))
    say(turn, "steered:%s" % prompt_of(params))
    finish(turn)


def scenario_idle_steer(turn, prompt):
    """The turn ends with a subagent still running, so the run is still open; a steering message
    that now meets the idle host starts a turn of its own, which the run waits for."""
    begin_turn(turn, prompt)
    sub = uid()
    emit_item("started", "subagent", sub, turn, 1, "inProgress", objective="x", subagentId="s1")
    say(turn, "first turn done")
    finish(turn)
    frame = wait_for("turn/start")
    params = frame["params"]
    late = params["commandId"]
    ack(frame, params, late, "started")
    begin_turn(late, prompt_of(params))
    say(late, "steered:%s" % prompt_of(params))
    finish(late)
    emit_item("completed", "subagent", sub, turn, 2, "completed", objective="x", subagentId="s1",
              result={"summary": "done"})


def scenario_interrupt(turn, prompt):
    begin_turn(turn, prompt)
    say(turn, "waiting")
    if os.environ.get("FAKE_MUSE_IGNORE_INTERRUPT"):
        while True:
            time.sleep(1)
    frame = wait_for("turn/interrupt")
    reply(frame, {"commandId": frame["params"]["commandId"], "status": "accepted"})
    finish(turn, "cancelled", reason="turn cancelled by the user")


def scenario_crash(turn, prompt):
    begin_turn(turn, prompt)
    say(turn, "about to crash")
    sys.stderr.write("fake-muse: fatal: simulated crash\n")
    sys.stderr.flush()
    sys.exit(3)


def scenario_failed(turn, prompt):
    begin_turn(turn, prompt)
    say(turn, "partial")
    finish(turn, "failed", error={"kind": "quota", "message": "monthly quota used up",
                                  "retryable": False})


def scenario_unknown(turn, prompt):
    begin_turn(turn, prompt)
    write({"jsonrpc": "2.0", "method": "turn/teleported", "params": {"sessionId": SESSION}})
    emit_item("completed", "hologram", uid(), turn, 1, "completed")
    say(turn, "unknown-done")
    finish(turn)


def scenario_mcp(turn, prompt):
    begin_turn(turn, prompt)
    say(turn, "mcp configured:%s" % ",".join(MCP_SERVERS))
    finish(turn)


def scenario_subagent(turn, prompt):
    """The turn ends while a subagent still runs; Muse starts a turn of its own when it ends."""
    begin_turn(turn, prompt)
    sub = uid()
    emit_item("started", "subagent", sub, turn, 1, "inProgress", role="hello agent",
              objective="Reply with exactly: Hello", subagentId="subagent-1")
    say(turn, "spawned")
    finish(turn)
    time.sleep(0.4)
    # The subagent's own message names the child's turn: not the run's, and its delta no more.
    child_item = uid()
    emit_item("started", "agentMessage", child_item, "child-turn", 1, "inProgress", text="")
    note("item/delta", itemId=child_item, field="text", delta="child-secret-text")
    emit_item("completed", "subagent", sub, turn, 2, "completed", role="hello agent",
              objective="Reply with exactly: Hello", subagentId="subagent-1",
              result={"summary": "Hello"})
    wake = uid()
    begin_turn(wake, "")
    say(wake, "subagent said Hello")
    finish(wake)


def scenario_subagent_no_wake(turn, prompt):
    """Background work ends and Muse starts no turn for it."""
    begin_turn(turn, prompt)
    sub = uid()
    emit_item("started", "subagent", sub, turn, 1, "inProgress", objective="x", subagentId="s1")
    say(turn, "spawned")
    finish(turn)
    time.sleep(0.2)
    emit_item("completed", "subagent", sub, turn, 2, "completed", objective="x", subagentId="s1",
              result={"summary": "done"})


def scenario_subagent_hang(turn, prompt):
    begin_turn(turn, prompt)
    emit_item("started", "subagent", uid(), turn, 1, "inProgress", objective="x", subagentId="s1")
    say(turn, "spawned")
    finish(turn)
    frame = wait_for("turn/interrupt", timeout=30)
    reply(frame, {"commandId": frame["params"]["commandId"], "status": "accepted"})


def scenario_resumed(turn, prompt):
    begin_turn(turn, prompt)
    say(turn, "back again")
    finish(turn)


def scenario_slow(turn, prompt):
    begin_turn(turn, prompt)
    say(turn, "still going")
    while True:
        time.sleep(1)


def scenario_sleep_then_echo(turn, prompt):
    begin_turn(turn, prompt)
    say(turn, "ok")
    finish(turn)


SCENARIOS = {
    "echo": scenario_echo,
    "conf-happy": scenario_conf_happy,
    "child": scenario_child,
    "approval": scenario_approval,
    "conf-approval-subjects": scenario_conf_approval,
    "staged": scenario_staged,
    "only-broad": scenario_only_broad,
    "conf-question": scenario_question,
    "question": scenario_question,
    "steer": scenario_steer,
    "idle-steer": scenario_idle_steer,
    "interrupt": scenario_interrupt,
    "conf-crash": scenario_crash,
    "failed": scenario_failed,
    "conf-unknown": scenario_unknown,
    "mcp-config": scenario_mcp,
    "subagent": scenario_subagent,
    "subagent-no-wake": scenario_subagent_no_wake,
    "subagent-hang": scenario_subagent_hang,
    "resumed": scenario_resumed,
    "slow": scenario_slow,
    "ok": scenario_sleep_then_echo,
}

MCP_SERVERS = []


def start_turn(frame, params):
    global TURNS, MCP_SERVERS
    text = prompt_of(params)
    turn = params["commandId"]
    TURNS += 1
    if text.startswith("scenario:refuse"):
        fail(frame, "commandRejected", "turn/start refused by the fake", -32030)
        return
    # An instructions block rides the prompt ahead of the task.
    body = text.split("</instructions>", 1)[-1].strip()
    name = body.removeprefix("scenario:").split()[0] if body.startswith("scenario:") else None
    if params.get("ifBusy") == "steer" and name is None:
        # A steering message that meets an idle host starts a turn of its own.
        ack(frame, params, turn)
        begin_turn(turn, text)
        say(turn, "steered:%s" % text)
        finish(turn)
        return
    ack(frame, params, turn)
    play = SCENARIOS.get(name or "ok", scenario_sleep_then_echo)
    play(turn, text)


# The session's MCP servers, as the fake saw them at `session/start`.
_handle = handle


def handle(frame):  # noqa: F811
    global MCP_SERVERS
    if frame.get("method") in ("session/start", "session/resume"):
        servers = (frame.get("params", {}).get("config") or {}).get("mcpServers", {})
        MCP_SERVERS = list(servers.keys())
    _handle(frame)


while True:
    frame = next_frame()
    if frame is None:
        break
    handle(frame)
