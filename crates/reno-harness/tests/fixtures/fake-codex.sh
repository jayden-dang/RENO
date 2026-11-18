#!/bin/sh
# Fake Codex app-server for reno-harness tests.
#
# Speaks scripted JSON-RPC 2.0 over stdio: initialize handshake, thread
# start/resume, then a scenario picked from the turn/start prompt text. Driven
# by crates/reno-harness/tests/codex.rs and crates/reno-kernel/tests/codex_e2e.rs.
#
# The approval request shapes follow the app-server protocol of codex-cli 0.164.0
# (codex-rs/app-server-protocol, v2/item.rs and v2/mcp.rs). They are hand-written, not
# captures: no fixture here is a live transcript except tests/fixtures/codex/live-*.jsonl.

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

fail_turn() { # $1 = request id, $2 = message
  emit "{\"id\":$1,\"result\":{\"turn\":{\"id\":\"t-bad\"}}}"
  emit "{\"method\":\"turn/failed\",\"params\":{\"turn\":{\"id\":\"t-bad\",\"error\":{\"message\":\"$2\"}}}}"
}

# A completed agent message: the run's result text (quotes and backslashes dropped, so the
# JSON stays valid whatever a diagnostic line carried).
say() {
  text=$(printf '%s' "$1" | tr -d '"\\')
  emit "{\"method\":\"item/completed\",\"params\":{\"item\":{\"id\":\"final\",\"type\":\"agentMessage\",\"text\":\"$text\"}}}"
}

# `--version`, for the driver's probe.
case "$1" in
  --version) echo "codex-cli ${FAKE_CODEX_VERSION:-0.169.0}"; exit 0 ;;
esac
# The launch flags, for scenario:user-config.
argv="$*"

# ---- handshake -------------------------------------------------------------
read -r line || exit 1 # initialize
has "$line" '"method":"initialize"' || exit 1
has "$line" '"experimentalApi":true' || exit 1
has "$line" '"name":"reno-native"' || exit 1
if [ -n "$FAKE_CODEX_REJECT_OPT_OUT" ] && has "$line" 'optOutNotificationMethods'; then
  # An app-server that predates notification opt-outs: refuse, then expect
  # the handshake again without them.
  emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32602,\"message\":\"unknown field optOutNotificationMethods\"}}"
  read -r line || exit 1
  has "$line" '"method":"initialize"' || exit 1
  has "$line" 'optOutNotificationMethods' && exit 7
fi
init_line="$line"
emit "{\"id\":$(rid "$line"),\"result\":{\"userAgent\":\"fake-codex\"}}"

read -r line || exit 1 # initialized notification (no reply)
has "$line" '"method":"initialized"' || exit 1

# ---- probe: account/read, in the state FAKE_CODEX_ACCOUNT names ----------------
# (chatgpt, apikey, signedout, noauth, error, hang). Only a probe sends it, right after
# `initialized`; a run goes straight to thread/start.
read -r line || exit 1
if has "$line" '"method":"account/read"'; then
  has "$line" '"refreshToken":false' || exit 1
  case "${FAKE_CODEX_ACCOUNT:-chatgpt}" in
    chatgpt) emit "{\"id\":$(rid "$line"),\"result\":{\"account\":{\"type\":\"chatgpt\",\"email\":\"me@example.com\",\"planType\":\"pro\"},\"requiresOpenaiAuth\":true}}" ;;
    apikey) emit "{\"id\":$(rid "$line"),\"result\":{\"account\":{\"type\":\"apiKey\"},\"requiresOpenaiAuth\":true}}" ;;
    signedout) emit "{\"id\":$(rid "$line"),\"result\":{\"account\":null,\"requiresOpenaiAuth\":true}}" ;;
    noauth) emit "{\"id\":$(rid "$line"),\"result\":{\"account\":null,\"requiresOpenaiAuth\":false}}" ;;
    hang) exec sleep 30 ;;
    *) emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32601,\"message\":\"method not found\"}}" ;;
  esac
  read -r line || exit 1
fi
# ---- thread start / resume -------------------------------------------------
thread_line="$line"
if has "$line" 'crash-on-thread-start'; then
  # The app-server dies under thread/start: the run must report why.
  echo 'thread 1 panicked at config.rs: bad toml in mcp_servers' >&2
  exit 3
fi
if has "$line" '"method":"model/list"'; then
  # Live model discovery: force pagination and put the default model second,
  # proving the driver consumes nextCursor and honors isDefault.
  has "$line" '"includeHidden":false' || exit 1
  has "$line" '"limit":20' || exit 1
  emit "{\"id\":$(rid "$line"),\"result\":{\"data\":[{\"id\":\"gpt-5.6-terra\",\"model\":\"gpt-5.6-terra\",\"displayName\":\"GPT-5.6-Terra\",\"description\":\"Balanced agentic coding model for everyday work.\",\"hidden\":false,\"supportedReasoningEfforts\":[{\"reasoningEffort\":\"low\"},{\"reasoningEffort\":\"high\"}],\"additionalSpeedTiers\":[],\"serviceTiers\":[],\"defaultServiceTier\":null,\"isDefault\":false},{\"id\":\"gpt-hidden\",\"model\":\"gpt-hidden\",\"displayName\":\"Hidden\",\"hidden\":true,\"isDefault\":false},{\"id\":\"gpt-6-astra\",\"model\":\"gpt-6-astra\",\"displayName\":\"GPT-6-Astra\",\"description\":\"Our most capable model for complex, demanding work.\",\"hidden\":false,\"supportedReasoningEfforts\":[{\"reasoningEffort\":\"low\"},{\"reasoningEffort\":\"ultra\"}],\"additionalSpeedTiers\":[\"fast\"],\"serviceTiers\":[{\"id\":\"priority\",\"name\":\"Fast\"}],\"defaultServiceTier\":null,\"isDefault\":true}],\"nextCursor\":\"page-2\"}}"
  read -r line || exit 1
  has "$line" '"method":"model/list"' || exit 1
  has "$line" '"cursor":"page-2"' || exit 1
  emit "{\"id\":$(rid "$line"),\"result\":{\"data\":[{\"id\":\"gpt-5.6-sol\",\"model\":\"gpt-5.6-sol\",\"displayName\":\"GPT-5.6-Sol\",\"description\":\"Reliable agentic workhorse for everyday tasks.\",\"hidden\":false,\"supportedReasoningEfforts\":[{\"reasoningEffort\":\"low\"},{\"reasoningEffort\":\"ultra\"}],\"additionalSpeedTiers\":[],\"serviceTiers\":[],\"defaultServiceTier\":null,\"isDefault\":false}],\"nextCursor\":null}}"
  exec sleep 30
fi
if has "$line" '"method":"thread/resume"'; then
  if has "$line" '"threadId":"resume-fail"'; then
    # Missing rollout (live 0.164.0 wording): reject, expect the fresh-start
    # fallback.
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32600,\"message\":\"no rollout found for thread id resume-fail\"}}"
    read -r line || exit 1
    has "$line" '"method":"thread/start"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-fresh\"}}}"
  elif has "$line" '"threadId":"resume-empty"'; then
    # An empty rollout file (live 0.164.0 wording): unreadable for good, so
    # the same fresh-start fallback.
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32603,\"message\":\"failed to read thread: thread-store internal error: failed to read session metadata /h/sessions/2026/10/23/rollout-2026-10-23T10-00-00-resume-empty.jsonl: rollout at /h/sessions/2026/10/23/rollout-2026-10-23T10-00-00-resume-empty.jsonl is empty\"}}"
    read -r line || exit 1
    has "$line" '"method":"thread/start"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-fresh\"}}}"
  elif has "$line" '"threadId":"resume-archived"'; then
    # Archived (live 0.164.0 wording): expect thread/unarchive, then the
    # resume again.
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32600,\"message\":\"session resume-archived is archived. Run \`codex unarchive resume-archived\` to unarchive it first.\"}}"
    read -r line || exit 1
    has "$line" '"method":"thread/unarchive"' && has "$line" '"threadId":"resume-archived"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"resume-archived\"}}}"
    read -r line || exit 1
    has "$line" '"method":"thread/resume"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-unarchived\"}}}"
  elif has "$line" '"threadId":"resume-in-plan"'; then
    # A thread left in plan mode (live 0.164.0 resume shape).
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-resumed\"},\"model\":\"gpt-resumed\",\"reasoningEffort\":\"medium\",\"collaborationMode\":{\"mode\":\"plan\",\"settings\":{\"model\":\"gpt-resumed\",\"reasoning_effort\":\"medium\",\"developer_instructions\":\"# Plan Mode\"}}}}"
  elif has "$line" '"threadId":"resume-broken"'; then
    # A failure a fresh thread would share: no thread/start may follow.
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32603,\"message\":\"failed to load configuration\"}}"
    while read -r line; do
      has "$line" '"method":"thread/start"' && emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-wrongly-fresh\"}}}"
    done
    exit 0
  else
    # A resume continues the thread it was asked for: the same id comes back.
    tid=$(printf '%s' "$line" | sed -n 's/.*"threadId":"\([^"]*\)".*/\1/p')
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"$tid\"}}}"
  fi
elif has "$line" '"method":"thread/start"'; then
  emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-1\"}}}"
else
  exit 1
fi

# ---- first turn ------------------------------------------------------------
read -r turnline || exit 1
tid=$(rid "$turnline")

case "$turnline" in
*scenario:lease-shutdown*)
  # Keep the child alive until the driver escalates and reaps it: it ignores SIGTERM.
  trap '' TERM
  printf '%s\n' "$$" > lease-child.pid
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  exec sleep 30
  ;;

*scenario:image-*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"item/started","params":{"threadId":"th-1","item":{"id":"image-1","type":"imageGeneration","status":"in_progress","result":""}}}'
  if has "$turnline" 'scenario:image-success'; then
    emit '{"method":"item/completed","params":{"threadId":"th-1","item":{"id":"image-1","type":"imageGeneration","status":"completed","result":"INLINE_IMAGE_SENTINEL","savedPath":"/codex/generated_images/goblin.png"}}}'
  elif has "$turnline" 'scenario:image-failure'; then
    emit '{"method":"item/completed","params":{"threadId":"th-1","item":{"id":"image-1","type":"imageGeneration","status":"failed","result":"INLINE_IMAGE_SENTINEL","failure":{"type":"usageLimitExceeded"}}}}'
  else
    emit '{"method":"item/completed","params":{"threadId":"th-1","item":{"id":"image-1","type":"imageGeneration","status":"completed","result":"INLINE_IMAGE_SENTINEL"}}}'
  fi
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:view-image*)
  # `view_image` (live 0.164.0 shape): started and completed arrive together.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"item/started","params":{"item":{"type":"imageView","id":"exec-view-1","path":"/Users/me/Documents/Codex/mockup.png"},"threadId":"th-1","turnId":"t-1","startedAtMs":1792848136768},"emittedAtMs":1792848136769}'
  emit '{"method":"item/completed","params":{"item":{"type":"imageView","id":"exec-view-1","path":"/Users/me/Documents/Codex/mockup.png"},"threadId":"th-1","turnId":"t-1","completedAtMs":1792848136769},"emittedAtMs":1792848136772}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"The preview is above."}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:reasoning*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"item/started","params":{"threadId":"th-1","item":{"id":"call_alpha","type":"subAgentActivity","kind":"spawned","agentThreadId":"child-1","agentPath":"/root/alpha"}}}'
  emit '{"method":"item/reasoning/summaryPartAdded","params":{"threadId":"th-1","itemId":"r1","summaryIndex":0}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"th-1","itemId":"r1","summaryIndex":0,"delta":"**Implementing file"}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"child-1","itemId":"r1","summaryIndex":0,"delta":"**Checking"}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"th-1","itemId":"r1","summaryIndex":0,"delta":" badges**"}}'
  emit '{"method":"item/reasoning/summaryPartAdded","params":{"threadId":"th-1","itemId":"r1","summaryIndex":1}}'
  emit '{"method":"item/reasoning/summaryPartAdded","params":{"threadId":"th-1","itemId":"r1","summaryIndex":1}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"th-1","itemId":"r1","summaryIndex":1,"delta":"**Preparing fixture screenshots**"}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"child-1","itemId":"r1","summaryIndex":0,"delta":" layout**"}}'
  emit '{"method":"item/reasoning/summaryPartAdded","params":{"threadId":"child-1","itemId":"r1","summaryIndex":1}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"child-1","itemId":"r1","delta":"Inspecting the output panel."}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"th-1","itemId":"r2","summaryIndex":0,"delta":"Checking the final result."}}'
  emit '{"method":"turn/completed","params":{"threadId":"child-1","turn":{"id":"ct-1"}}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  ;;

*scenario:happy*)
  # Verify the turn/start + thread/start params the driver must send.
  for want in '"method":"turn/start"' '"effort":"high"' '"model":"gpt-5.6-sol"' \
    '"sandboxPolicy":{"type":"dangerFullAccess"}' \
    '"approvalPolicy":"untrusted"' '"approvalsReviewer":"user"' '"summary":"auto"'; do
    has "$turnline" "$want" || { fail_turn "$tid" "turn param missing: $want"; exit 0; }
  done
  # The prompt is one text input, verbatim.
  has "$turnline" '"type":"text"' && has "$turnline" '"text":"scenario:happy"' ||
    { fail_turn "$tid" "turn input is not the prompt"; exit 0; }
  for want in '"approvalPolicy":"untrusted"' '"approvalsReviewer":"user"' '"sandbox":"danger-full-access"' '"cwd":"/tmp"'; do
    has "$thread_line" "$want" || { fail_turn "$tid" "thread param missing: $want"; exit 0; }
  done
  # Full access asks (so Reno's kernel can refuse what policy denies), and no mode is named for
  # a thread that never planned.
  for unwanted in 'collaborationMode'; do
    if has "$thread_line" "$unwanted" || has "$turnline" "$unwanted"; then
      fail_turn "$tid" "full access sent $unwanted"; exit 0
    fi
  done
  for want in '"optOutNotificationMethods":[' '"turn/diff/updated"' '"item/fileChange/outputDelta"'; do
    has "$init_line" "$want" || { fail_turn "$tid" "initialize opt-out missing: $want"; exit 0; }
  done
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  # Deltas: both field spellings must be accepted.
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"Hello"}}'
  emit '{"method":"item/reasoning/textDelta","params":{"itemId":"r1","textDelta":"thinking hard"}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"itemId":"r1","delta":"summary"}}'
  # Item lifecycles.
  emit '{"method":"item/started","params":{"item":{"id":"c1","type":"commandExecution","command":"ls -la"}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"c1","type":"commandExecution","command":"ls -la","status":"completed","exitCode":1,"aggregatedOutput":"first\nno such file\n"}}}'
  emit '{"method":"item/started","params":{"item":{"id":"f1","type":"fileChange","changes":[{"path":"/tmp/new.rs","kind":"add"}]}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"f1","type":"fileChange","status":"completed","changes":[{"path":"/tmp/new.rs","kind":"add"}]}}}'
  emit '{"method":"item/started","params":{"item":{"id":"mcp1","type":"mcpToolCall","server":"linear","tool":"search","arguments":{"q":"bug"}}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"mcp1","type":"mcpToolCall","server":"linear","tool":"search","status":"failed"}}}'
  emit '{"method":"item/started","params":{"item":{"id":"w1","type":"webSearch","query":"rust"}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"w1","type":"webSearch","query":"rust"}}}'
  # Completion-only lifecycle: must still open AND close the tool call.
  emit '{"method":"item/completed","params":{"item":{"id":"td1","type":"todoList","items":[{"text":"a","completed":true},{"text":"b","completed":false}]}}}'
  # Streamed agentMessage: completed text must NOT re-emit.
  emit '{"method":"item/completed","params":{"item":{"id":"m1","type":"agentMessage","text":"Hello world"}}}'
  # Never-streamed agentMessage: completed text is the fallback delta.
  emit '{"method":"item/completed","params":{"item":{"id":"m2","type":"agentMessage","text":"unstreamed tail"}}}'
  # Unknown notification methods must be tolerated.
  emit '{"method":"some/unknownNotification","params":{"x":1}}'
  emit '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"inputTokens":42,"outputTokens":7,"totalTokens":49},"modelContextWindow":258400}}}'
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:child-identity*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  cat "$(dirname "$0")/codex/child-identity.jsonl"
  ;;

*scenario:v1-subagents*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  cat "$(dirname "$0")/codex/v1-subagents.jsonl"
  ;;

*scenario:v2-lifecycle*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  cat "$(dirname "$0")/codex/v2-lifecycle.jsonl"
  ;;

*scenario:live-multi-agent*)
  # The notifications of a live codex-cli 0.146.1 multi-agent capture, replayed. Its first
  # two lines are responses (initialize, thread/start), which this script already sent.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  # The capture's root thread is this fake's `th-1`; its two children keep their own ids.
  grep '^{"method"' "$(dirname "$0")/codex/live-0.146.1-multi-agent.jsonl" |
    sed 's/01a00fe7-c301-7d52-ae1f-41ecda48b29b/th-1/g'
  ;;

*scenario:subagent*)
  # Multi-agent v2 child-thread routing: the child's items and turn bookkeeping must never
  # reach the parent's feed, nor settle the parent turn.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"item/started","params":{"threadId":"th-1","item":{"id":"call_alpha","type":"subAgentActivity","kind":"started","agentThreadId":"child-1","agentPath":"/root/alpha"}}}'
  emit '{"method":"item/completed","params":{"threadId":"th-1","item":{"id":"call_alpha","type":"subAgentActivity","kind":"started","agentThreadId":"child-1","agentPath":"/root/alpha"}}}'
  # The wire also emits subAgentActivity about the ROOT during collab runs.
  emit '{"method":"item/started","params":{"threadId":"th-1","item":{"id":"call_root","type":"subAgentActivity","kind":"interacted","agentThreadId":"th-1","agentPath":"/root"}}}'
  emit '{"method":"thread/status/changed","params":{"threadId":"child-1","status":{"type":"running"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"child-1","itemId":"cm1","delta":"child says hi"}}'
  emit '{"method":"item/started","params":{"threadId":"child-1","item":{"id":"cs1","type":"commandExecution","command":"echo hi"}}}'
  emit '{"method":"item/completed","params":{"threadId":"child-1","item":{"id":"cs1","type":"commandExecution","command":"echo hi","status":"completed","exitCode":0}}}'
  emit '{"method":"item/started","params":{"threadId":"child-1","item":{"id":"cu1","type":"userMessage","text":"also check the rebuild"}}}'
  emit '{"method":"item/completed","params":{"threadId":"child-1","item":{"id":"cu1","type":"userMessage","text":"also check the rebuild"}}}'
  # The child settles ITS turn: the parent turn must keep running.
  emit '{"method":"turn/completed","params":{"threadId":"child-1","turn":{"id":"ct-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"parent still going"}}'
  # An unknown method addressed to the child falls through, never vanishes.
  emit '{"method":"thread/somethingBrandNew","params":{"threadId":"child-1"}}'
  emit '{"method":"thread/closed","params":{"threadId":"child-1"}}'
  emit '{"method":"item/completed","params":{"threadId":"th-1","item":{"id":"subagent-completed-alpha","type":"subAgentActivity","kind":"completed","agentThreadId":"child-1","agentPath":"/root/alpha"}}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  ;;

*scenario:child-approval*)
  # A child thread asks for approval: the request is Reno's to answer, like the parent's.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":151,"method":"item/commandExecution/requestApproval","params":{"threadId":"child-1","turnId":"ct-1","itemId":"cc1","startedAtMs":1,"command":"make deploy","cwd":"/work"}}'
  read -r a1 || exit 1
  { has "$a1" '"id":151' && has "$a1" '"decision":"decline"'; } ||
    { emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"child command not declined"}}}}'; exit 0; }
  emit '{"method":"turn/completed","params":{"threadId":"child-1","turn":{"id":"ct-1"}}}'
  say "child refused"
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  ;;

# NOTE: steer-race before steer: `case` takes the first matching glob.
*scenario:steer-item*)
  # Live shape (0.159.3): an accepted turn/steer joins the turn only when the reply streaming
  # ahead of it finishes; its userMessage item carries the clientUserMessageId it was sent
  # with as `clientId`.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"first"}}'
  read -r steerline || exit 1
  has "$steerline" '"method":"turn/steer"' || exit 8
  cid=$(printf '%s' "$steerline" | sed 's/.*"clientUserMessageId":"\([^"]*\)".*/\1/')
  [ -n "$cid" ] && [ "$cid" != "$steerline" ] || exit 8
  emit "{\"id\":$(rid "$steerline"),\"result\":{\"turnId\":\"t-1\"}}"
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"-still-first"}}'
  emit '{"method":"item/completed","params":{"threadId":"th-1","item":{"type":"agentMessage","id":"m1","text":"first-still-first"}}}'
  emit "{\"method\":\"item/started\",\"params\":{\"threadId\":\"th-1\",\"item\":{\"type\":\"userMessage\",\"id\":\"u2\",\"clientId\":\"$cid\",\"content\":[{\"type\":\"text\",\"text\":\"redirect please\"}]}}}"
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m2","delta":"steered"}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:stale-completion*)
  # A completion for a turn other than the one in flight must not settle it.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"before"}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-zombie","status":"completed"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"after"}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  # A duplicate of the settled turn's completion is stale too.
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:effort-next-turn*)
  # The first turn is rejected a steer (as in steer-race); the follow-up turn/start must carry
  # the effort changed meanwhile.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  emit "{\"id\":$sid,\"error\":{\"code\":-32602,\"message\":\"turn already completed\"}}"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  read -r followline || exit 1
  fid=$(rid "$followline")
  emit "{\"id\":$fid,\"result\":{\"turn\":{\"id\":\"t-2\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-2"}}}'
  if has "$followline" '"effort":"low"'; then
    emit '{"method":"item/agentMessage/delta","params":{"itemId":"m2","delta":"low"}}'
  elif has "$followline" '"effort":"high"'; then
    emit '{"method":"item/agentMessage/delta","params":{"itemId":"m2","delta":"high"}}'
  else
    emit '{"method":"item/agentMessage/delta","params":{"itemId":"m2","delta":"none"}}'
  fi
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-2"}}}'
  ;;

*scenario:steer-race*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"turn/steer"' ||
    { emit "{\"id\":$sid,\"result\":{}}"; emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected turn/steer"}}}}'; exit 0; }
  # The turn completed under the steer: reject, then announce completion.
  emit "{\"id\":$sid,\"error\":{\"code\":-32602,\"message\":\"turn already completed\"}}"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  # The driver must fall back to a follow-up turn/start carrying the text, and the run must
  # not end at the first turn's completion.
  read -r followline || exit 1
  fid=$(rid "$followline")
  if has "$followline" '"method":"turn/start"' && has "$followline" 'redirect please'; then
    emit "{\"id\":$fid,\"result\":{\"turn\":{\"id\":\"t-2\"}}}"
    emit '{"method":"turn/started","params":{"turn":{"id":"t-2"}}}'
    emit '{"method":"item/agentMessage/delta","params":{"itemId":"m2","delta":"fallback"}}'
    emit '{"method":"turn/completed","params":{"turn":{"id":"t-2"}}}'
  else
    fail_turn "$fid" "expected fallback turn/start with steer text"
  fi
  ;;

*scenario:steer*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  if has "$steerline" '"method":"turn/steer"' &&
    has "$steerline" '"expectedTurnId":"t-1"' &&
    has "$steerline" 'redirect please'; then
    emit "{\"id\":$sid,\"result\":{}}"
    emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"steered"}}'
    emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  else
    emit "{\"id\":$sid,\"error\":{\"code\":-32600,\"message\":\"bad steer\"}}"
    emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"steer verification failed"}}}}'
  fi
  ;;

*scenario:one-approval*)
  # One command approval; the result text says how the app-server saw the answer. Shared by
  # the harness tests and the kernel end-to-end test.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":701,"method":"item/commandExecution/requestApproval","params":{"threadId":"th-1","turnId":"t-1","itemId":"c1","startedAtMs":1,"command":"rm -rf build","cwd":"/work","reason":"clean the build"}}'
  read -r a1 || exit 1
  if has "$a1" '"id":701' && has "$a1" '"decision":"accept"'; then
    say "ran"
  elif has "$a1" '"id":701' && has "$a1" '"decision":"decline"'; then
    say "refused"
  else
    say "unexpected: $a1"
  fi
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:approve*)
  # Supervised: codex asks before anything it does not know to be safe, in a read-only sandbox.
  # Each approval is a structured request the driver surfaces and the test decides.
  for want in '"approvalPolicy":"untrusted"' '"sandbox":"read-only"' '"approvalsReviewer":"user"'; do
    has "$thread_line" "$want" || { fail_turn "$tid" "supervised thread param missing: $want"; exit 0; }
  done
  for want in '"approvalPolicy":"untrusted"' '"sandboxPolicy":{"type":"readOnly"}' '"approvalsReviewer":"user"'; do
    has "$turnline" "$want" || { fail_turn "$tid" "supervised turn param missing: $want"; exit 0; }
  done
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"id":101,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"rm -rf /tmp/x","cwd":"/tmp"}}'
  read -r a1 || exit 1
  has "$a1" '"id":101' || { say "lost 101"; exit 0; }
  if has "$a1" '"decision":"accept"'; then first=accept; else first=decline; fi
  # The file change shape of the 0.164.0 schema: the request names only the item; its paths
  # came with the item's own started notification.
  emit '{"method":"item/started","params":{"item":{"id":"f1","type":"fileChange","status":"inProgress","changes":[{"path":"/tmp/a.rs","diff":"","kind":{"type":"update"}}]}}}'
  emit '{"id":102,"method":"item/fileChange/requestApproval","params":{"threadId":"th-1","turnId":"t-1","itemId":"f1","startedAtMs":2,"reason":"write outside the sandbox"}}'
  read -r a2 || exit 1
  has "$a2" '"id":102' || { say "lost 102"; exit 0; }
  if has "$a2" '"decision":"accept"'; then second=accept; else second=decline; fi
  say "command=$first file=$second"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:file-change-inline*)
  # An older app-server put the changes in the request itself.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"id":111,"method":"item/fileChange/requestApproval","params":{"itemId":"f9","changes":[{"path":"/tmp/a.rs","kind":"update"},{"path":"/tmp/b.rs","kind":"add"}]}}'
  read -r a1 || exit 1
  if has "$a1" '"decision":"accept"'; then say "file=accept"; else say "file=decline"; fi
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:auto-edits*)
  # Auto-accept edits: the workspace is writable (network on) and codex asks about everything
  # it does not know to be safe, file changes included. A file change whose paths are all
  # inside the working directory is accepted without asking; everything else is asked.
  cwd=$(printf '%s' "$thread_line" | sed 's/.*"cwd":"\([^"]*\)".*/\1/')
  for want in '"approvalPolicy":"untrusted"' '"sandbox":"workspace-write"' '"approvalsReviewer":"user"'; do
    has "$thread_line" "$want" || { fail_turn "$tid" "auto-edits thread param missing: $want"; exit 0; }
  done
  for want in '"approvalPolicy":"untrusted"' '"type":"workspaceWrite"' '"networkAccess":true' '"approvalsReviewer":"user"'; do
    has "$turnline" "$want" || { fail_turn "$tid" "auto-edits turn param missing: $want"; exit 0; }
  done
  has "$turnline" 'collaborationMode' && { fail_turn "$tid" "collaborationMode without plan"; exit 0; }
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  # 401: inside the working directory (one absolute path, one relative): accepted, not asked.
  emit "{\"method\":\"item/started\",\"params\":{\"item\":{\"id\":\"f1\",\"type\":\"fileChange\",\"changes\":[{\"path\":\"$cwd/src/a.rs\",\"kind\":\"update\"},{\"path\":\"b.rs\",\"kind\":\"add\"}]}}}"
  emit '{"id":401,"method":"item/fileChange/requestApproval","params":{"itemId":"f1","startedAtMs":1}}'
  read -r a1 || exit 1
  { has "$a1" '"id":401' && has "$a1" '"decision":"accept"'; } || { say "401 not accepted: $a1"; exit 0; }
  # 402: outside: asked.
  emit '{"method":"item/started","params":{"item":{"id":"f2","type":"fileChange","changes":[{"path":"/outside/a.rs","kind":"update"}]}}}'
  emit '{"id":402,"method":"item/fileChange/requestApproval","params":{"itemId":"f2","startedAtMs":2}}'
  read -r a2 || exit 1
  { has "$a2" '"id":402' && has "$a2" '"decision":"decline"'; } || { say "402 not declined: $a2"; exit 0; }
  # 403: a path that climbs out through `..`: asked.
  emit "{\"method\":\"item/started\",\"params\":{\"item\":{\"id\":\"f3\",\"type\":\"fileChange\",\"changes\":[{\"path\":\"$cwd/../escape.rs\",\"kind\":\"add\"}]}}}"
  emit '{"id":403,"method":"item/fileChange/requestApproval","params":{"itemId":"f3","startedAtMs":3}}'
  read -r a3 || exit 1
  { has "$a3" '"id":403' && has "$a3" '"decision":"decline"'; } || { say "403 not declined: $a3"; exit 0; }
  # 404: an item the driver never saw: no paths known, so asked.
  emit '{"id":404,"method":"item/fileChange/requestApproval","params":{"itemId":"never-seen","startedAtMs":4}}'
  read -r a4 || exit 1
  { has "$a4" '"id":404' && has "$a4" '"decision":"decline"'; } || { say "404 not declined: $a4"; exit 0; }
  # 405: inside, but a session-wide write grant: asked.
  emit "{\"method\":\"item/started\",\"params\":{\"item\":{\"id\":\"f5\",\"type\":\"fileChange\",\"changes\":[{\"path\":\"$cwd/c.rs\",\"kind\":\"update\"}]}}}"
  emit "{\"id\":405,\"method\":\"item/fileChange/requestApproval\",\"params\":{\"itemId\":\"f5\",\"startedAtMs\":5,\"grantRoot\":\"$cwd\"}}"
  read -r a5 || exit 1
  { has "$a5" '"id":405' && has "$a5" '"decision":"decline"'; } || { say "405 not declined: $a5"; exit 0; }
  # 406: a command: asked.
  emit '{"id":406,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"git push","cwd":"/work"}}'
  read -r a6 || exit 1
  { has "$a6" '"id":406' && has "$a6" '"decision":"decline"'; } || { say "406 not declined: $a6"; exit 0; }
  # 407: a permission grant: asked, and refused with an empty grant.
  emit '{"id":407,"method":"item/permissions/requestApproval","params":{"itemId":"p1","cwd":"/work","reason":"fetch","permissions":{"network":{"enabled":true}}}}'
  read -r a7 || exit 1
  { has "$a7" '"id":407' && has "$a7" '"permissions":{}'; } || { say "407 not refused: $a7"; exit 0; }
  say "all asked"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:edits-through-links*)
  # Auto-accept edits, with symlinks: the test made `$cwd/real` (a directory) and `$cwd/link` (a
  # symlink to a directory outside). A path is inside only if it still is once links are followed.
  cwd=$(printf '%s' "$thread_line" | sed 's/.*"cwd":"\([^"]*\)".*/\1/')
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  n=0
  for want_path in "accept|$cwd/real/a.rs" "decline|$cwd/link/b.rs" "decline|$cwd/link/../c.rs" \
    "accept|$cwd/real/new/deep/d.rs" "decline|$cwd/missing/../e.rs"; do
    want=${want_path%%|*}; path=${want_path#*|}; n=$((n + 1))
    emit "{\"method\":\"item/started\",\"params\":{\"item\":{\"id\":\"f$n\",\"type\":\"fileChange\",\"changes\":[{\"path\":\"$path\",\"kind\":\"add\"}]}}}"
    emit "{\"id\":$((500 + n)),\"method\":\"item/fileChange/requestApproval\",\"params\":{\"itemId\":\"f$n\",\"startedAtMs\":$n}}"
    read -r reply || exit 1
    { has "$reply" "\"id\":$((500 + n))" && has "$reply" "\"decision\":\"$want\""; } ||
      { say "request $n ($path) not answered $want: $reply"; emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'; exit 0; }
  done
  say "links resolved"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:decline*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"id":201,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":["rm","-rf","/"],"cwd":"/"}}'
  read -r a1 || exit 1
  { has "$a1" '"id":201' && has "$a1" '"decision":"decline"'; } ||
    { emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected decline"}}}}'; exit 0; }
  # Never `acceptForSession`: Reno decides every time.
  has "$a1" 'ForSession' && { emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"session-wide decision"}}}}'; exit 0; }
  say "declined"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:mcp-approval*)
  # An MCP tool call's approval is an elicitation whose `_meta` names its kind (0.164.0
  # codex-rs/core/src/mcp_tool_call.rs): a form with no fields.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":801,"method":"mcpServer/elicitation/request","params":{"threadId":"th-1","turnId":"t-1","serverName":"reno","mode":"form","_meta":{"codex_approval_kind":"mcp_tool_call","persist":["session","always"],"tool_name":"board_post","tool_title":"Post to the board","tool_params":{"text":"hi"}},"message":"Allow the reno MCP server to run tool \"board_post\"?","requestedSchema":{"type":"object","properties":{}}}}'
  read -r a1 || exit 1
  has "$a1" '"id":801' || { say "lost 801"; exit 0; }
  # A persisting answer would amend Codex's own policy; only a plain accept or decline is sent.
  has "$a1" 'persist' && { say "persisting answer"; exit 0; }
  if has "$a1" '"action":"accept"'; then say "tool=accept"; else say "tool=decline"; fi
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:user-input*)
  # `item/tool/requestUserInput` (experimental): the agent asks the person a question.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":901,"method":"item/tool/requestUserInput","params":{"threadId":"th-1","turnId":"t-1","itemId":"u1","questions":[{"id":"mode","header":"Mode","question":"Which mode?","isOther":false,"options":[{"label":"fast","description":"quick"},{"label":"safe","description":"careful"}]}]}}'
  read -r a1 || exit 1
  has "$a1" '"id":901' || { say "lost 901"; exit 0; }
  if has "$a1" '"answers":{"mode":{"answers":["safe"]}}'; then
    say "mode=safe"
  elif has "$a1" '"answers":{}'; then
    say "mode=none"
  else
    say "unexpected: $a1"
  fi
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:elicitation-form*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":303,"method":"mcpServer/elicitation/request","params":{"threadId":"th-1","serverName":"deploy","mode":"form","message":"Ship it?","requestedSchema":{"type":"object","required":["confirm"],"properties":{"confirm":{"type":"boolean","title":"Confirm"}}}}}'
  read -r r3 || exit 1
  has "$r3" '"id":303' || { say "lost 303"; exit 0; }
  if has "$r3" '"action":"accept"' && has "$r3" '"confirm":true'; then say "form=accepted"
  elif has "$r3" '"action":"accept"' && has "$r3" '"confirm":false'; then say "form=accepted-false"
  elif has "$r3" '"action":"decline"'; then say "form=declined"
  else say "unexpected: $r3"; fi
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:server-requests*)
  # Requests the driver answers without asking anyone: an elicitation it cannot show, and a
  # method it does not know.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":302,"method":"mcpServer/elicitation/request","params":{"threadId":"th-1","serverName":"auth","mode":"url","message":"Sign in","url":"https://example.invalid","elicitationId":"e1"}}'
  read -r r2 || exit 1
  { has "$r2" '"id":302' && has "$r2" '"action":"decline"' && ! has "$r2" '"error"'; } ||
    { emit '{"method":"turn/completed","params":{"turn":{"id":"t-1","status":"failed","error":{"message":"url elicitation not declined"}}}}'; exit 0; }
  emit '{"id":304,"method":"brand/newRequest","params":{}}'
  read -r r4 || exit 1
  { has "$r4" '"id":304' && has "$r4" '"code":-32601'; } ||
    { emit '{"method":"turn/completed","params":{"turn":{"id":"t-1","status":"failed","error":{"message":"unknown request not refused"}}}}'; exit 0; }
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:permissions*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":301,"method":"item/permissions/requestApproval","params":{"threadId":"th-1","turnId":"t-1","itemId":"p1","cwd":"/tmp","startedAtMs":1,"reason":"fetch deps","permissions":{"network":{"enabled":true},"fileSystem":{"write":["/repo/.cache"]}}}}'
  read -r r1 || exit 1
  has "$r1" '"id":301' || { say "lost 301"; exit 0; }
  if has "$r1" '"permissions":{}'; then
    say "permissions=none"
  elif has "$r1" '"network":{"enabled":true}' && has "$r1" '"write":["/repo/.cache"]' && has "$r1" '"scope":"turn"'; then
    say "permissions=turn"
  else
    say "unexpected: $r1"
  fi
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:open-then-cancel*)
  # An approval is open when the run is cancelled: it must be declined before anything else.
  cwd=$(printf '%s' "$thread_line" | sed 's/.*"cwd":"\([^"]*\)".*/\1/')
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"id":501,"method":"item/commandExecution/requestApproval","params":{"threadId":"th-1","turnId":"t-1","itemId":"c1","startedAtMs":1,"command":"make","cwd":"/work"}}'
  read -r a1 || exit 1
  { has "$a1" '"id":501' && has "$a1" '"decision":"decline"'; } || { printf 'wrong: %s\n' "$a1" > "$cwd/open-request.txt"; exit 0; }
  printf 'declined\n' > "$cwd/open-request.txt"
  read -r intline || exit 1
  has "$intline" '"method":"turn/interrupt"' || exit 9
  emit "{\"id\":$(rid "$intline"),\"result\":{}}"
  emit '{"method":"turn/aborted","params":{"turn":{"id":"t-1"}}}'
  cat >/dev/null
  ;;

*scenario:request-after-cancel*)
  # A request that arrives after the run was cancelled is declined at once.
  cwd=$(printf '%s' "$thread_line" | sed 's/.*"cwd":"\([^"]*\)".*/\1/')
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"working"}}'
  read -r intline || exit 1
  has "$intline" '"method":"turn/interrupt"' || exit 9
  emit "{\"id\":$(rid "$intline"),\"result\":{}}"
  emit '{"id":601,"method":"item/commandExecution/requestApproval","params":{"threadId":"th-1","turnId":"t-1","itemId":"c1","startedAtMs":1,"command":"make","cwd":"/work"}}'
  emit '{"id":602,"method":"item/tool/requestUserInput","params":{"threadId":"th-1","turnId":"t-1","itemId":"u1","questions":[{"id":"q","header":"Q","question":"Why?","options":[]}]}}'
  read -r a1 || exit 1
  read -r a2 || exit 1
  { has "$a1" '"id":601' && has "$a1" '"decision":"decline"'; } || { printf 'wrong: %s\n' "$a1" > "$cwd/late-request.txt"; exit 0; }
  { has "$a2" '"id":602' && has "$a2" '"answers":{}'; } || { printf 'wrong: %s\n' "$a2" > "$cwd/late-request.txt"; exit 0; }
  printf 'declined\n' > "$cwd/late-request.txt"
  emit '{"method":"turn/aborted","params":{"turn":{"id":"t-1"}}}'
  cat >/dev/null
  ;;

*scenario:interrupt*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}'
  read -r intline || exit 1
  iid=$(rid "$intline")
  if has "$intline" '"method":"turn/interrupt"' && has "$intline" '"turnId":"t-1"'; then
    emit "{\"id\":$iid,\"result\":{}}"
    emit '{"method":"turn/aborted","params":{"turn":{"id":"t-1"}}}'
  else
    emit "{\"id\":$iid,\"result\":{}}"
    emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected turn/interrupt"}}}}'
  fi
  ;;

*scenario:silent-start*)
  # The app-server never answers turn/start: a cancel must still end the run.
  exec sleep 30
  ;;

*scenario:wedge*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}'
  # Ignore turn/interrupt entirely: forces the SIGTERM escalation path.
  exec sleep 30
  ;;

*scenario:retry-errors*)
  # Live shapes (0.164.0 schema): retries announce themselves as `error` with willRetry; the
  # final error repeats as turn.error.
  resets=$(( $(date +%s) + 2 * 3600 + 600 ))
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit "{\"method\":\"account/rateLimits/updated\",\"params\":{\"rateLimits\":{\"primary\":{\"usedPercent\":100,\"windowDurationMins\":300,\"resetsAt\":$resets},\"secondary\":null}}}"
  emit '{"method":"error","params":{"threadId":"th-1","turnId":"t-1","willRetry":true,"error":{"message":"Reconnecting... 1/5 (rate limit)","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":429}}}}}'
  emit '{"method":"error","params":{"threadId":"th-1","turnId":"t-1","willRetry":true,"error":{"message":"Reconnecting... 2/5 (rate limit)","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":429}}}}}'
  emit '{"method":"error","params":{"threadId":"th-1","turnId":"t-1","willRetry":false,"error":{"message":"You have hit your usage limit.","codexErrorInfo":"usageLimitExceeded"}}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"failed","error":{"message":"You have hit your usage limit.","codexErrorInfo":"usageLimitExceeded"}}}}'
  ;;

*scenario:crash-after-error*)
  # A final error, then the app-server dies before the turn's end.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"error","params":{"threadId":"th-1","turnId":"t-1","willRetry":false,"error":{"message":"unexpected status 401 Unauthorized: Incorrect API key provided","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":401}}}}}'
  echo 'thread main panicked at session.rs: lost the model client' >&2
  exit 3
  ;;

*scenario:foreign-interrupt*)
  # Interrupted by another client of the thread, not by us.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","itemId":"m1","delta":"half an ans"}}'
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"interrupted"}}}'
  ;;

*scenario:mcp-config*)
  # The thread's config overrides carry the injected servers. Reno's own (the first) is
  # pre-approved only when nothing is asked of the person.
  for want in '"mcp_servers.reno.command":"/opt/reno/reno"' '"mcp_servers.reno.args":["mcp"]' \
    '"mcp_servers.reno.env":{"RENO_RUN":"r1"}' '"mcp_servers.reno.tool_timeout_sec":3720' \
    '"mcp_servers.reno.startup_timeout_sec":30' \
    '"mcp_servers.plugin-focus.command":"/opt/focus/server"'; do
    has "$thread_line" "$want" || { fail_turn "$tid" "mcp override missing: $want"; exit 0; }
  done
  has "$thread_line" 'mcp_servers.plugin-focus.default_tools_approval_mode' &&
    { fail_turn "$tid" "a plugin's tools were pre-approved"; exit 0; }
  if has "$thread_line" '"approvalPolicy":"never"'; then
    has "$thread_line" '"mcp_servers.reno.default_tools_approval_mode":"approve"' ||
      { fail_turn "$tid" "full access should pre-approve Reno's own tools"; exit 0; }
  else
    has "$thread_line" 'mcp_servers.reno.default_tools_approval_mode' &&
      { fail_turn "$tid" "Reno's tools were pre-approved while the person is asked"; exit 0; }
  fi
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  say "configured"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:user-config*)
  # Isolated: every personal feature off and the owner's server (from the CODEX_HOME the
  # scenario names, tests/fixtures/codex/owner-home) disabled by name. Inherited: none of it.
  isolated=yes
  inherited=yes
  for want in 'features.apps=false' 'features.plugins=false' 'features.computer_use=false' \
    'features.browser_use=false' 'mcp_servers.owner-tool.enabled=false'; do
    if has "$argv" "$want"; then inherited=no; else isolated=no; fi
  done
  has "$argv" 'mcp_servers.reno.enabled' && { fail_turn "$tid" "Reno's own server was disabled"; exit 0; }
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  if [ "$isolated" = yes ]; then say "user-config:isolated"
  elif [ "$inherited" = yes ]; then say "user-config:inherited"
  else say "user-config:partial $argv"; fi
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:policy*)
  # Report what the thread and the turn were launched with, as the run's result.
  ap=$(printf '%s' "$thread_line" | sed 's/.*"approvalPolicy":"\([^"]*\)".*/\1/')
  sb=$(printf '%s' "$thread_line" | sed 's/.*"sandbox":"\([^"]*\)".*/\1/')
  tp=$(printf '%s' "$turnline" | sed 's/.*"approvalPolicy":"\([^"]*\)".*/\1/')
  # Keys are in alphabetical order on the wire: `networkAccess` comes before `type`.
  ts=$(printf '%s' "$turnline" | sed 's/.*"sandboxPolicy":{[^}]*"type":"\([^"]*\)".*/\1/')
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  say "thread=$ap/$sb turn=$tp/$ts"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:fail*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"boom"}}}}'
  ;;

*scenario:leave-plan*)
  # A resumed thread still in plan mode, on a run that is not: the turn names `default`, with
  # the model and effort the thread reported.
  for want in '"collaborationMode":{' '"mode":"default"' '"model":"gpt-resumed"' '"reasoning_effort":"medium"'; do
    has "$turnline" "$want" || { fail_turn "$tid" "leave-plan turn param missing: $want"; exit 0; }
  done
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:resumed*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:conf-happy*)
  # Conformance: text, a command and a file change, a result. No parameter is checked.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"threadId":"th-1","turnId":"t-1","itemId":"m1","delta":"conf-hello"}}'
  emit '{"method":"item/started","params":{"threadId":"th-1","turnId":"t-1","item":{"id":"c1","type":"commandExecution","command":"ls -la"}}}'
  emit '{"method":"item/completed","params":{"threadId":"th-1","turnId":"t-1","item":{"id":"c1","type":"commandExecution","command":"ls -la","status":"completed","exitCode":0,"aggregatedOutput":"ok\n"}}}'
  emit '{"method":"item/started","params":{"threadId":"th-1","turnId":"t-1","item":{"id":"f1","type":"fileChange","changes":[{"path":"/tmp/new.rs","diff":"","kind":{"type":"add"}}]}}}'
  emit '{"method":"item/completed","params":{"threadId":"th-1","turnId":"t-1","item":{"id":"f1","type":"fileChange","status":"completed","changes":[{"path":"/tmp/new.rs","diff":"","kind":{"type":"add"}}]}}}'
  say "conf-done"
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*scenario:conf-unknown*)
  # Conformance: a notification method and a server request from an app-server newer than
  # the driver.
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"threadId":"th-1","turn":{"id":"t-1"}}}'
  emit '{"method":"zz/futureNotification","params":{"threadId":"th-1","n":1}}'
  say "unknown-done"
  emit '{"method":"turn/completed","params":{"threadId":"th-1","turn":{"id":"t-1","status":"completed"}}}'
  ;;

*)
  fail_turn "$tid" "unknown scenario"
  ;;
esac
