#!/bin/sh
# Fake ACP agent for reno-harness tests.
#
# Speaks scripted JSON-RPC 2.0 over stdio: initialize handshake, session
# new/load, then a scenario picked from the session/prompt text. Driven by
# crates/harness/tests/acp.rs. Always advertises the `_session/steering`
# extension; the steer-queue scenario REJECTS steer requests to exercise the
# turn-boundary fallback (the Grok-without-extension path).

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

update() { # $1 = update json object body
  emit "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":$1}}"
}

# ---- handshake -------------------------------------------------------------
read -r line || exit 1 # initialize
has "$line" '"method":"initialize"' || exit 1
has "$line" '"protocolVersion":1' || exit 1
has "$line" '"name":"reno"' || exit 1
has "$line" '"readTextFile":false' || exit 1
emit "{\"id\":$(rid "$line"),\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true,\"_meta\":{\"availableCommands\":[{\"name\":\"compact\",\"description\":\"Compact the session\"},{\"name\":\"goal\",\"description\":\"Set a goal\",\"input\":{\"hint\":\"the goal\"}}]}},\"_meta\":{\"steering\":{\"supported\":true}}}}"

# ---- session new / load ----------------------------------------------------
read -r line || exit 1
SID="s-1"
MODEL_API=0
if has "$line" '"sessionId":"existing-grok-session"'; then
  MODEL_API=1
fi
if has "$line" '"method":"session/load"'; then
  if has "$line" '"sessionId":"load-fail"'; then
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32602,\"message\":\"unknown session\"}}"
    read -r line || exit 1
    has "$line" '"method":"session/new"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-fresh\"}}"
    SID="s-fresh"
  elif has "$line" '"sessionId":"load-hang"' || has "$line" '"sessionId":"load-busy"' ||
    has "$line" '"sessionId":"load-exit"'; then
    # A restore that never answers, is refused for a reason other than a
    # missing session, or loses the agent: none may fork to a fresh session.
    # Should a session/new arrive anyway, answer it so the fork shows.
    if has "$line" '"sessionId":"load-busy"'; then
      emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32603,\"message\":\"session store is locked\"}}"
    elif has "$line" '"sessionId":"load-exit"'; then
      exit 3
    fi
    while read -r line; do
      if has "$line" '"method":"session/new"'; then
        emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-forked\"}}"
      fi
    done
    exit 0
  else
    if [ "$MODEL_API" -eq 1 ]; then
      SID="existing-grok-session"
    else
      SID="s-loaded"
    fi
    # Replay history BEFORE the load response resolves: the harness must
    # drain these without emitting events (the doc already has them) and
    # without deadlocking on a full incoming channel.
    i=0
    if has "$line" '"sessionId":"mcp-loaded"'; then
      SID="mcp-loaded"
      i=300
    fi
    while [ $i -lt 300 ]; do
      update '{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old prompt"}}'
      update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old reply"}}'
      i=$((i+1))
    done
    if [ "$MODEL_API" -eq 1 ]; then
      emit "{\"id\":$(rid "$line"),\"result\":{\"models\":{\"currentModelId\":\"grok-4-fast\",\"availableModels\":[{\"modelId\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\"},{\"modelId\":\"grok-4.5\",\"name\":\"Grok 4.5\"}]}}}"
    else
      emit "{\"id\":$(rid "$line"),\"result\":{}}"
    fi
  fi
elif has "$line" '"method":"session/new"'; then
  has "$line" '"mcpServers":[]' || has "$line" '"name":"reno"' || exit 1
  # Advertise config options: model (current differs from the tests' request,
  # forcing a set) and thought_level (current high). The model config option
  # feeds discovery first; the first-class `models` state (SessionModelState)
  # is the legacy fallback — codex-acp enumerates model × effort there.
  if [ "$MODEL_API" -eq 1 ]; then
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-1\",\"models\":{\"currentModelId\":\"grok-4-fast\",\"availableModels\":[{\"modelId\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\"},{\"modelId\":\"grok-4.5\",\"name\":\"Grok 4.5\"}]}}}"
  else
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"s-1\",\"models\":{\"availableModels\":[{\"modelId\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\",\"description\":\"Fast tier\"},{\"modelId\":\"grok-4.5\",\"name\":\"Grok 4.5\"}],\"currentModelId\":\"grok-4.5\"},\"configOptions\":[{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"grok-4-fast\",\"options\":[{\"value\":\"grok-4-fast\",\"name\":\"Grok 4 Fast\",\"description\":\"Fast tier\"},{\"value\":\"grok-4.5\",\"name\":\"Grok 4.5\"}]},{\"id\":\"effort\",\"name\":\"Reasoning effort\",\"category\":\"thought_level\",\"type\":\"select\",\"currentValue\":\"high\",\"options\":[{\"value\":\"low\",\"name\":\"Low\"},{\"value\":\"medium\",\"name\":\"Medium\"},{\"value\":\"high\",\"name\":\"High\"}]}]}}"
  fi
else
  exit 1
fi

if [ -f .command-fixture ]; then
  expected=$(pwd -P)
  has "$line" "\"cwd\":\"$expected\"" || exit 1
  name=$(cat .command-fixture)
  emit "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":{\"sessionUpdate\":\"available_commands_update\",\"availableCommands\":[{\"name\":\"$name\",\"description\":\"Project command\"}]}}}"
fi

# ---- model/config sets (0..n), then the first turn ---------------------------
CONFIG_SETS=""
MODEL_SETS=""
read -r promptline || exit 1
while has "$promptline" '"method":"session/set_config_option"' \
  || has "$promptline" '"method":"session/set_model"'; do
  emit "{\"id\":$(rid "$promptline"),\"result\":{}}"
  if has "$promptline" '"method":"session/set_model"'; then
    MODEL_SETS="$MODEL_SETS $promptline"
  else
    CONFIG_SETS="$CONFIG_SETS $promptline"
  fi
  read -r promptline || exit 1
done
has "$promptline" '"method":"session/prompt"' || exit 1
pid=$(rid "$promptline")

case "$promptline" in

*scenario:mcp*)
  has "$line" '"command":"/path with spaces/reno"' || exit 1
  has "$line" '"args":["mcp"]' || exit 1
  has "$line" '"name":"RENO_CHAT_ID","value":"origin-chat"' || exit 1
  has "$line" '"name":"RENO_IPC_PORT","value":"27699"' || exit 1
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"mcp configured"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:model-api*)
  if has "$MODEL_SETS" '"modelId":"grok-4.5"'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"model switched"}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:config*)
  # The tests' request carries model grok-4.5 + medium effort; both differ
  # from the advertised currents, so both sets must have arrived.
  if has "$CONFIG_SETS" '"configId":"model"' && has "$CONFIG_SETS" '"value":"grok-4.5"' \
    && has "$CONFIG_SETS" '"configId":"effort"' && has "$CONFIG_SETS" '"value":"medium"'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"configured"}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:many*)
  # A wide read pass: 14 expandable chips — regression surface for the
  # analytic group-height accounting (auto-height cards clipped the tail).
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Reading the workspace."}}'
  i=1
  while [ $i -le 14 ]; do
    update "{\"sessionUpdate\":\"tool_call\",\"toolCallId\":\"r$i\",\"title\":\"read file $i\",\"kind\":\"read\",\"status\":\"completed\",\"rawInput\":{\"path\":\"/w/src/file_$i.rs\"},\"content\":[{\"type\":\"content\",\"content\":{\"type\":\"text\",\"text\":\"contents of file $i\"}}]}"
    i=$((i+1))
  done
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:prompt-complete-hang*)
  # The grok field hang: the turn really finishes — prompt_complete fires
  # with the echoed _meta.promptId — but the session/prompt RPC is NEVER
  # answered. The harness must settle off the notification.
  reqpid=$(printf '%s' "$promptline" | sed 's/.*"promptId":"\([^"]*\)".*/\1/')
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"pong"}}'
  # grok 1.0.88 settles the turn's usage on its extension channel just before.
  emit "{\"method\":\"_x.ai/session_notification\",\"params\":{\"sessionId\":\"$SID\",\"update\":{\"sessionUpdate\":\"turn_completed\",\"prompt_id\":\"$reqpid\",\"stop_reason\":\"end_turn\",\"usage\":{\"inputTokens\":1200,\"outputTokens\":30,\"cachedReadTokens\":200,\"cacheCreationTokens\":0}}}}"
  emit "{\"method\":\"_x.ai/session/prompt_complete\",\"params\":{\"sessionId\":\"$SID\",\"promptId\":\"$reqpid\",\"stopReason\":\"end_turn\",\"agentResult\":null}}"
  # Hang the response forever (the wedge under test).
  exec sleep 60
  ;;

*scenario:turn-usage-twice*)
  # Both channels carry the turn's usage (the RPC _meta and turn_completed), and a replay of the
  # turn_completed arrives after the turn settled: the turn is counted once.
  reqpid=$(printf '%s' "$promptline" | sed 's/.*"promptId":"\([^"]*\)".*/\1/')
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"pong"}}'
  tc="{\"method\":\"_x.ai/session_notification\",\"params\":{\"sessionId\":\"$SID\",\"update\":{\"sessionUpdate\":\"turn_completed\",\"prompt_id\":\"$reqpid\",\"usage\":{\"inputTokens\":1200,\"outputTokens\":30,\"cachedReadTokens\":200,\"cacheCreationTokens\":0}}}}"
  emit "$tc"
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"inputTokens\":1200,\"outputTokens\":30,\"cachedReadTokens\":200}}}"
  emit "$tc"
  ;;

*scenario:prompt-complete-stale*)
  # A STALE completion (wrong prompt id — a replay of an already-settled
  # prompt) must not settle the live turn; the real response follows.
  emit "{\"method\":\"_x.ai/session/prompt_complete\",\"params\":{\"sessionId\":\"$SID\",\"promptId\":\"stale-p0\",\"stopReason\":\"cancelled\",\"agentResult\":null}}"
  # A completion for ANOTHER session is equally inert.
  emit "{\"method\":\"_x.ai/session/prompt_complete\",\"params\":{\"sessionId\":\"other\",\"promptId\":\"reno-p1\",\"stopReason\":\"cancelled\",\"agentResult\":null}}"
  sleep 1
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"real answer"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"inputTokens\":9,\"outputTokens\":4}}}"
  ;;

*scenario:prompt-stall*)
  # Session boilerplate, then silence — the REAL wedge signature. opencode
  # emits available_commands_update on every session (provider dead or not),
  # so the watchdog must not count it as turn progress: exactly this frame
  # used to disarm the watchdog for the whole turn (found live, 1.18.18).
  emit "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"$SID\",\"update\":{\"sessionUpdate\":\"available_commands_update\",\"availableCommands\":[{\"name\":\"init\",\"description\":\"guided AGENTS.md setup\"}]}}}"
  exec sleep 60
  ;;

*scenario:detached*)
  # A command the agent starts in its own session (Devin does): not in the
  # agent's process group, so a group kill alone leaves it running.
  pidfile=$(printf '%s' "$promptline" | sed 's/.*scenario:detached \([^ "]*\).*/\1/')
  python3 -c "import os,time; os.setsid(); open('$pidfile','w').write(str(os.getpid())); time.sleep(60)" &
  while [ ! -s "$pidfile" ]; do sleep 0.05; done
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"started"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  cat >/dev/null
  ;;

*scenario:happy*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}}'
  update '{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}'
  # Non-text content chunks map to nothing.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"x","mimeType":"image/png"}}'
  # Execute tool: pending call, then completed update with output content.
  update '{"sessionUpdate":"tool_call","toolCallId":"t1","title":"cargo test -p reno-harness","kind":"execute","status":"pending","rawInput":{"command":"cargo test -p reno-harness"}}'
  update '{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"   Compiling reno-harness v0.1.21\n    Finished `dev` profile [unoptimized] in 2.41s\n     Running tests/acp.rs\n\nrunning 13 tests\ntest result: ok. 13 passed; 0 failed; 0 ignored"}}]}'
  # Edit tool resolved in one shot with an inline diff (real hunk: context,
  # line numbers, rust syntax for the transcript's diff component).
  update '{"sessionUpdate":"tool_call","toolCallId":"t2","title":"edit resolve.rs","kind":"edit","status":"completed","content":[{"type":"diff","path":"/w/src/resolve.rs","oldText":"use std::path::PathBuf;\n\n/// Locate the agent binary.\nfn resolve(exe: &str) -> Option<PathBuf> {\n    std::env::var_os(\"PATH\")\n        .map(PathBuf::from)\n        .filter(|p| p.exists())\n}\n","newText":"use std::path::PathBuf;\n\n/// Locate the agent binary.\nfn resolve(exe: &str) -> Option<PathBuf> {\n    let dirs = std::env::split_paths(&std::env::var_os(\"PATH\")?);\n    dirs.map(|d| d.join(exe)).find(|p| p.exists())\n}\n"}]}'
  # Plan → todo chip.
  update '{"sessionUpdate":"plan","entries":[{"content":"read","priority":"high","status":"completed"},{"content":"fix","priority":"high","status":"in_progress"}]}'
  # Command advertisement mid-run.
  update '{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"deep-research","description":"Research deeply"}]}'
  # Context gauge + unknown kinds are tolerated, another session filtered.
  update '{"sessionUpdate":"usage_update","used":1200,"size":500000}'
  update '{"sessionUpdate":"totally_unknown_kind","x":1}'
  emit '{"method":"session/update","params":{"sessionId":"other","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"WRONG SESSION"}}}}'
  emit '{"method":"some/unknownNotification","params":{"x":1}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:echo-prompt*)
  # Echo the prompt's first text block back as a delta (prompt-transform
  # verification: Ultrathink prefix must be on the wire).
  text=$(printf '%s' "$promptline" | sed 's/.*"text":"\([^"]*\)".*/\1/')
  update "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"$text\"}}"
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:question*)
  # AskUserQuestion-shaped request: options WITHOUT allow/reject kinds are
  # user-facing choices — must round-trip through the input bridge, never
  # auto-accept. The test's bridge answers "Use tokio".
  emit "{\"id\":88,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"toolCall\":{\"toolCallId\":\"q1\",\"title\":\"Which async runtime should I use?\"},\"options\":[{\"optionId\":\"opt-tokio\",\"name\":\"Use tokio\"},{\"optionId\":\"opt-smol\",\"name\":\"Use smol\"}]}}"
  read -r ans || exit 1
  { has "$ans" '"id":88' && has "$ans" '"outcome":"selected"' && has "$ans" '"optionId":"opt-tokio"'; } ||
    { emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"; exit 0; }
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answered"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:permission-hold*)
  # An approval left open when the run is cancelled, and one that arrives after the cancel.
  # ACP has the client answer every pending permission request `cancelled` when it cancels
  # the prompt; a request raised after the cancel is cancelled at once. The reply says what
  # the agent was told.
  emit "{\"id\":91,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"toolCall\":{\"toolCallId\":\"h1\",\"kind\":\"execute\",\"rawInput\":{\"command\":\"make\"}},\"options\":[{\"optionId\":\"once\",\"name\":\"Allow once\",\"kind\":\"allow_once\"},{\"optionId\":\"no\",\"name\":\"Reject\",\"kind\":\"reject_once\"}]}}"
  read -r first || exit 1
  read -r second || exit 1
  has "$first$second" '"method":"session/cancel"' || exit 1
  open=other
  has "$first$second" '"id":91' && has "$first$second" '"outcome":"cancelled"' && open=cancelled
  emit "{\"id\":92,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"toolCall\":{\"toolCallId\":\"h2\",\"kind\":\"execute\",\"rawInput\":{\"command\":\"late\"}},\"options\":[{\"optionId\":\"once\",\"name\":\"Allow once\",\"kind\":\"allow_once\"}]}}"
  read -r late || exit 1
  late_outcome=other
  has "$late" '"id":92' && has "$late" '"outcome":"cancelled"' && late_outcome=cancelled
  update "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"open:$open;late:$late_outcome\"}}"
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"cancelled\"}}"
  ;;

*scenario:permission*)
  # One tool permission, of the kind named after the scenario (`scenario:permission fetch`;
  # `execute` when none is given). Every option kind is offered, `allow_always` first: the
  # reply says which option the client selected.
  kind=$(printf '%s' "$promptline" | sed -n 's/.*scenario:permission \([a-z_]*\).*/\1/p')
  [ -n "$kind" ] || kind=execute
  emit "{\"id\":77,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"toolCall\":{\"toolCallId\":\"t1\",\"kind\":\"$kind\",\"title\":\"A $kind call\",\"rawInput\":{\"command\":\"rm -rf build\"},\"locations\":[{\"path\":\"/w/a.rs\"},{\"path\":\"/w/b.rs\",\"line\":3}]},\"options\":[{\"optionId\":\"always\",\"name\":\"Always allow\",\"kind\":\"allow_always\"},{\"optionId\":\"once\",\"name\":\"Allow once\",\"kind\":\"allow_once\"},{\"optionId\":\"never\",\"name\":\"Never allow\",\"kind\":\"reject_always\"},{\"optionId\":\"no\",\"name\":\"Reject\",\"kind\":\"reject_once\"}]}}"
  read -r ans || exit 1
  has "$ans" '"id":77' || exit 1
  case "$ans" in
    *'"optionId":"'*) picked=$(printf '%s' "$ans" | sed 's/.*"optionId":"\([^"]*\)".*/\1/') ;;
    *) picked=cancelled ;;
  esac
  update "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"picked:$picked\"}}"
  # The refusal's reason, when the client attached one (`_meta.denyMessage`).
  case "$ans" in
    *'"denyMessage":"'*)
      reason=$(printf '%s' "$ans" | sed 's/.*"denyMessage":"\([^"]*\)".*/\1/')
      update "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"reason:$reason\"}}"
      ;;
  esac
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:steer-ext*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  if has "$steerline" '"method":"_session/steering"' &&
    has "$steerline" 'redirect please' &&
    has "$steerline" '"idleBehavior":"promptRequired"'; then
    emit "{\"id\":$sid,\"result\":{\"outcome\":\"injected\"}}"
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"steered"}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$sid,\"error\":{\"code\":-32600,\"message\":\"bad steer\"}}"
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:steer-race*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"_session/steering"' || exit 1
  # The exact turn-end race: the injection lands in the turn's tail and the
  # PROMPT response hits the wire before the steering response does. The
  # harness must settle the steering call at the boundary — Steered before
  # Done, never after — instead of stranding the session or re-prompting.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"steered"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  sleep 0.2
  emit "{\"id\":$sid,\"result\":{\"outcome\":\"injected\"}}"
  ;;

*scenario:steer-queue*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"_session/steering"' || exit 1
  # Reject: the harness must queue the text and deliver it as the next
  # session/prompt at the turn boundary (the no-extension path).
  emit "{\"id\":$sid,\"error\":{\"code\":-32601,\"message\":\"steering unsupported\"}}"
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  read -r followline || exit 1
  fid=$(rid "$followline")
  if has "$followline" '"method":"session/prompt"' && has "$followline" 'redirect please'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"boundary"}}'
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:starve*)
  # The wedge: the prompt's turn was consumed by CLI-side
  # self-continuation and its response NEVER comes. Steering then answers
  # noRunningTurn — the harness must settle the dead turn after its grace
  # and promote the queued steer to a fresh session/prompt.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"_session/steering"' || exit 1
  emit "{\"id\":$sid,\"result\":{\"outcome\":\"promptRequired\",\"reason\":\"noRunningTurn\"}}"
  # No response to $pid, ever. The next line must be the promoted prompt.
  read -r followline || exit 1
  fid=$(rid "$followline")
  if has "$followline" '"method":"session/prompt"' && has "$followline" 'what about now'; then
    update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"promoted"}}'
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"end_turn\"}}"
  else
    emit "{\"id\":$fid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:interrupt*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  read -r intline || exit 1
  if has "$intline" '"method":"session/cancel"'; then
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"cancelled\"}}"
  else
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  fi
  ;;

*scenario:wedge*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  # Ignore session/cancel entirely — forces the SIGTERM escalation path.
  exec sleep 30
  ;;

*scenario:refusal*)
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  ;;

*scenario:conf-happy*)
  # Conformance: text, a command with its result, an end of turn.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"conf-hello"}}'
  update '{"sessionUpdate":"tool_call","toolCallId":"t1","title":"ls -la","kind":"execute","status":"pending","rawInput":{"command":"ls -la"}}'
  update '{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"ok"}}]}'
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"conf-done"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:conf-question*)
  # Conformance: a choice between options; any option answers it.
  emit "{\"id\":88,\"method\":\"session/request_permission\",\"params\":{\"sessionId\":\"$SID\",\"toolCall\":{\"toolCallId\":\"q1\",\"title\":\"Which async runtime should I use?\"},\"options\":[{\"optionId\":\"opt-tokio\",\"name\":\"Use tokio\"},{\"optionId\":\"opt-smol\",\"name\":\"Use smol\"}]}}"
  read -r ans || exit 1
  has "$ans" '"id":88' || exit 1
  case "$ans" in
    *'"optionId":"'*) picked=$(printf '%s' "$ans" | sed 's/.*"optionId":"\([^"]*\)".*/\1/') ;;
    *) picked=none ;;
  esac
  update "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"answered:$picked\"}}"
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*scenario:conf-crash*)
  # Conformance: the agent dies in the middle of its turn.
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}'
  echo 'conformance crash context' >&2
  exit 23
  ;;

*scenario:resumed*)
  update '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"back again"}}'
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\"}}"
  ;;

*)
  emit "{\"id\":$pid,\"result\":{\"stopReason\":\"refusal\"}}"
  ;;
esac
