#!/bin/sh
# Fake Claude Code CLI for reno-harness tests.
#
# Reads the first stream-json user line from stdin, picks a scenario from the
# prompt text, and plays a scripted stream-json transcript on stdout —
# including control-channel round-trips read back from stdin. Frame shapes
# mirror live captures from CLI 2.1.228. Driven by
# crates/reno-harness/tests/claude.rs, and by the kernel's end-to-end test.

# Discovery commands the driver's probe runs (never a run): `--version`, and `auth status` in the
# sign-in state FAKE_CLAUDE_AUTH names (in, out, garbage, broken, hang).
case "$1" in
  --version) echo "${FAKE_CLAUDE_VERSION:-2.1.325} (Claude Code)"; exit 0 ;;
  auth)
    case "${FAKE_CLAUDE_AUTH:-in}" in
      in) echo '{"loggedIn":true,"authMethod":"claude.ai","email":"me@example.com"}'; exit 0 ;;
      out) echo '{"loggedIn":false}'; exit 1 ;;
      garbage) echo 'something else entirely'; exit 0 ;;
      hang) exec sleep 30 ;;
      *) exit 2 ;;
    esac
    ;;
esac

read -r first || exit 1
# A run opens with an `initialize` control request (the SDK handshake); the
# real CLI answers it before the first user line.
initialized=false
while :; do
  case "$first" in
    *'"request_id":"reno_initialize"'*)
      case "$first" in *'"perTaskStopAffordance":true'*) initialized=true ;; esac
      printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"reno_initialize","response":{}}}'
      read -r first || exit 1
      ;;
    *) break ;;
  esac
done

emit() { printf '%s\n' "$1"; }
uuid_of() { printf '%s\n' "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }

case "$first" in


*scenario:happy*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash","Read"],"cwd":"/tmp","session_id":"sess-1"}'
  # Re-emitted init mid-run (background-task wakeup): must be deduped.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash","Read"],"cwd":"/tmp","session_id":"sess-1"}'
  # Post-init system subtypes the 2.1.x CLI emits (thinking accounting,
  # session state): must be tolerated and dropped.
  emit '{"type":"system","subtype":"thinking_tokens","tokens":12}'
  emit '{"type":"system","subtype":"session_state_changed","state":"running"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"pondering"}}}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}}'
  # Subagent frames (parent_tool_use_id set): tagged, never in the parent feed.
  emit '{"type":"stream_event","parent_tool_use_id":"sub-1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"SUBAGENT"}}}'
  emit '{"type":"assistant","parent_tool_use_id":"sub-1","message":{"content":[{"type":"tool_use","id":"sub-tool","name":"Bash","input":{"command":"echo sub"}}]}}'
  emit '{"type":"user","parent_tool_use_id":"sub-1","message":{"content":[{"type":"tool_result","tool_use_id":"sub-tool","is_error":false}]}}'
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"Hello"},{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"ls -la"}},{"type":"tool_use","id":"tool-2","name":"mcp__linear__search","input":{"q":"bug"}}]}}'
  emit '{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":false},{"type":"tool_result","tool_use_id":"tool-2","is_error":true}]}}'
  # Informational rate-limit status: stays quiet.
  emit '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}'
  emit '{"type":"result","subtype":"success","result":"done!","errors":[],"usage":{"input_tokens":10,"output_tokens":20},"session_id":"sess-1","total_cost_usd":0.01}'
  ;;

*scenario:user-config*)
  # Reports whether the isolation from the owner's personal config reached the CLI: the setting
  # sources it was given and the two environment switches. `isolated` only when all three are
  # there, `inherited` only when none is.
  sources=all
  prev=
  for arg in "$@"; do
    [ "$prev" = "--setting-sources" ] && sources=$arg
    prev=$arg
  done
  switches=0
  [ "${CLAUDE_CODE_DISABLE_AUTO_MEMORY:-}" = 1 ] && switches=$((switches + 1))
  [ "${ENABLE_CLAUDEAI_MCP_SERVERS:-}" = false ] && switches=$((switches + 1))
  if [ "$sources" = "project,local" ] && [ "$switches" = 2 ]; then
    verdict=isolated
  elif [ "$sources" = all ] && [ "$switches" = 0 ]; then
    verdict=inherited
  else
    verdict="partial(sources=$sources,switches=$switches)"
  fi
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-uc"}'
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"user-config:'"$verdict"'"}]}}'
  emit '{"type":"result","subtype":"success","result":"user-config:'"$verdict"'","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-uc"}'
  ;;

*scenario:env*)
  # Prints the environment the child was given to the file the run's own env names, so a test
  # can read what reached it (tests/child_env.rs).
  env | sort > "$RENO_FAKE_ENV_OUT"
  # The arguments, one per line: the `--mcp-config` JSON names the env of the MCP servers the
  # real CLI would start, which is how Reno's own server gets its variables.
  printf '%s\n' "$@" > "$RENO_FAKE_ENV_OUT.args"
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-env"}'
  emit '{"type":"result","subtype":"success","result":"ok","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-env"}'
  ;;

*scenario:wake*)
  # Eager-done + wake, the live-verified 2.1.228 background-subagent shape:
  # the parent turn settles with result #1 while the subagent still runs;
  # tagged subagent traffic continues; the CLI then wakes with a second init
  # (SAME session id) and settles the wake turn with result #2.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-wake"}'
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"toolu_agent","name":"Agent","input":{"description":"background task","run_in_background":true}}]}}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"LAUNCHED"}}}'
  emit '{"type":"result","subtype":"success","result":"LAUNCHED","errors":[],"usage":{"input_tokens":5,"output_tokens":5},"session_id":"sess-wake"}'
  # Background subagent interior, after the eager done.
  emit '{"type":"stream_event","parent_tool_use_id":"toolu_agent","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"sub working"}}}'
  emit '{"type":"assistant","parent_tool_use_id":"toolu_agent","message":{"content":[{"type":"tool_use","id":"sub-t1","name":"Bash","input":{"command":"sleep 1"}}]}}'
  emit '{"type":"user","parent_tool_use_id":"toolu_agent","message":{"content":[{"type":"tool_result","tool_use_id":"sub-t1","is_error":false}]}}'
  # The wake turn: re-init (deduped), untagged output, second result.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-wake"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"subagent finished"}}}'
  emit '{"type":"result","subtype":"success","result":"wrapped up","errors":[],"usage":{"input_tokens":3,"output_tokens":3},"session_id":"sess-wake"}'
  ;;

*scenario:askuser*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-ask"}'
  # A plain tool permission request: must be auto-allowed.
  emit '{"type":"control_request","request_id":"cr-0","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}'
  read -r resp0 || exit 1
  case "$resp0" in
  *'"request_id":"cr-0"'*'"behavior":"allow"'*) ;;
  *)
    emit '{"type":"result","subtype":"error_during_execution","errors":["bash tool was not allowed"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
    exit 0
    ;;
  esac
  # AskUserQuestion: must be intercepted and answered via updatedInput.answers.
  emit '{"type":"control_request","request_id":"cr-1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"header":"Choice","question":"Pick one","options":["A","B"],"multiSelect":false}]}}}'
  read -r resp1 || exit 1
  case "$resp1" in
  *'"behavior":"allow"'*)
    case "$resp1" in
    *'"Pick one":"B"'*)
      emit '{"type":"result","subtype":"success","result":"answered","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
      ;;
    *)
      emit '{"type":"result","subtype":"error_during_execution","errors":["answers missing from updatedInput"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
      ;;
    esac
    ;;
  *)
    emit '{"type":"result","subtype":"error_during_execution","errors":["AskUserQuestion was denied"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
    ;;
  esac
  ;;

*scenario:closed-question*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-ask"}'
  # Questions closed unanswered come back as a deny, never an empty allow.
  emit '{"type":"control_request","request_id":"cr-1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"header":"Choice","question":"Pick one","options":["A","B"],"multiSelect":false}]}}}'
  read -r resp1 || exit 1
  case "$resp1" in
  *'"request_id":"cr-1"'*'"behavior":"deny"'*'dismissed'*)
    emit '{"type":"result","subtype":"success","result":"declined","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
    ;;
  *)
    emit '{"type":"result","subtype":"error_during_execution","errors":["closed questions were not denied"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
    ;;
  esac
  ;;

*scenario:steer*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-steer"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"first"}}}'
  # The queued steering user line, applied at "the step boundary" (here: now).
  read -r steer || exit 1
  case "$steer" in *'"priority":"now"'*) ;; *) exit 9 ;; esac
  # Old output continues until Claude actually consumes the input.
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"-still-first"}}}'
  emit "$steer"
  content=$(printf '%s\n' "$steer" | sed 's/.*"content":"\([^"]*\)".*/\1/')
  emit "{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"steered:$content\"}}}"
  emit '{"type":"result","subtype":"success","result":"steered","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-steer"}'
  ;;

*scenario:lifecycle-now-steer*)
  # CLI 2.1.286, captured live: every uuid-tagged user line gets
  # `command_lifecycle` frames. A `now` steer aborts the streaming turn
  # (result, terminal_reason aborted_streaming), the first command is
  # cancelled and the steer's own turn starts. That turn then runs a tool
  # for longer than any quiet-time heuristic: no Done may land before the
  # steered turn's own result.
  fid=$(uuid_of "$first")
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"queued\"}"
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"started\"}"
  emit '{"type":"system","subtype":"init","model":"claude-sonnet-5-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-lc"}'
  emit "$first"
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"essay"}}}'
  read -r steer || exit 1
  case "$steer" in *'"priority":"now"'*) ;; *) exit 9 ;; esac
  sid=$(uuid_of "$steer")
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"queued\"}"
  emit '{"type":"result","subtype":"success","is_error":false,"terminal_reason":"aborted_streaming","result":"essay","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-lc"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"cancelled\"}"
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"started\"}"
  emit '{"type":"system","subtype":"init","model":"claude-sonnet-5-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-lc"}'
  emit "$steer"
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"tool-sleep","name":"Bash","input":{"command":"sleep 8"}}]}}'
  sleep "${FAKE_CLAUDE_TOOL_SECS:-6}"
  emit '{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"tool-sleep","is_error":false}]}}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"DONE"}}}'
  emit '{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":"DONE","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-lc"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"completed\"}"
  ;;

*scenario:lifecycle-queued-behind-end*)
  # A message that reaches the CLI as its turn is finishing waits for that
  # turn's result, then starts its own turn: the first result is a
  # boundary, and the run's only Done is the queued message's.
  fid=$(uuid_of "$first")
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"started\"}"
  emit '{"type":"system","subtype":"init","model":"claude-sonnet-5-5","tools":[],"cwd":"/tmp","session_id":"sess-q"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"first answer"}}}'
  read -r steer || exit 1
  sid=$(uuid_of "$steer")
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"queued\"}"
  emit '{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":"first answer","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-q"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"completed\"}"
  sleep 1
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"started\"}"
  emit "$steer"
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"second answer"}}}'
  emit '{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":"second answer","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-q"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"completed\"}"
  ;;

*scenario:lifecycle-queued-cancelled*)
  # A queued message cancelled before any turn takes it up: the result held
  # for it was the turn's real end, released at once — no quiet timer.
  fid=$(uuid_of "$first")
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"started\"}"
  emit '{"type":"system","subtype":"init","model":"claude-sonnet-5-5","tools":[],"cwd":"/tmp","session_id":"sess-qc"}'
  read -r steer || exit 1
  sid=$(uuid_of "$steer")
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"queued\"}"
  emit '{"type":"result","subtype":"success","is_error":false,"result":"only","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-qc"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"completed\"}"
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$sid\",\"state\":\"cancelled\"}"
  exec sleep 30
  ;;

*scenario:approvals*)
  # One tool call of each kind through the permission channel; the result
  # says the launch's permission flags and how each call was answered.
  mode=none
  skip=none
  prev=
  for arg in "$@"; do
    [ "$prev" = "--permission-mode" ] && mode=$arg
    case "$arg" in *skip-permissions) skip=$arg ;; esac
    prev=$arg
  done
  emit "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fable-5\",\"tools\":[],\"cwd\":\"/tmp\",\"session_id\":\"sess-ap\",\"permissionMode\":\"$mode\"}"
  answer() {
    read -r resp || exit 1
    case "$resp" in
    *"\"request_id\":\"$1\""*'"behavior":"allow"'*'"mode":"'*)
      printf 'allow:%s' "$(printf '%s\n' "$resp" | sed 's/.*"mode":"\([^"]*\)".*/\1/')"
      ;;
    *"\"request_id\":\"$1\""*'"behavior":"allow"'*) printf allow ;;
    *"\"request_id\":\"$1\""*'"behavior":"deny"'*) printf deny ;;
    *) exit 1 ;;
    esac
  }
  emit '{"type":"control_request","request_id":"cr-bash","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf build"}}}'
  bash=$(answer cr-bash) || exit 1
  emit '{"type":"control_request","request_id":"cr-edit","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"/tmp/a.rs","old_string":"a","new_string":"b"}}}'
  edit=$(answer cr-edit) || exit 1
  emit '{"type":"control_request","request_id":"cr-plan","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"Do it."}}}'
  plan=$(answer cr-plan) || exit 1
  emit "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$mode $skip bash=$bash edit=$edit plan=$plan\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-ap\"}"
  ;;

*scenario:resume-killed-tasks*)
  # Captured live on 2.1.286: `--resume` of a session whose background tasks
  # died with its previous process. The CLI settles them with two empty
  # `result` frames (num_turns 0) while the new prompt is still queued —
  # they are not that prompt's end.
  fid=$(uuid_of "$first")
  emit '{"type":"system","subtype":"task_notification","task_id":"a-1","status":"stopped"}'
  emit '{"type":"system","subtype":"task_notification","task_id":"b-1","status":"stopped"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"queued\"}"
  emit '{"type":"system","subtype":"init","model":"claude-sonnet-5-5","tools":[],"cwd":"/tmp","session_id":"sess-rk"}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"","errors":[],"usage":{"input_tokens":0,"output_tokens":0},"session_id":"sess-rk"}'
  emit '{"type":"system","subtype":"init","model":"claude-sonnet-5-5","tools":[],"cwd":"/tmp","session_id":"sess-rk"}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"","errors":[],"usage":{"input_tokens":0,"output_tokens":0},"session_id":"sess-rk"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"started\"}"
  emit "$first"
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"PONG"}}}'
  emit '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"terminal_reason":"completed","result":"PONG","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-rk"}'
  emit "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$fid\",\"state\":\"completed\"}"
  ;;

*scenario:superseded-steers*)
  # CLI 2.1.280 with rapid `now` steers: the second interrupts the turn the
  # first started before it is replayed; only the last steer is replayed.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-sup"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"story"}}}'
  read -r steer1 || exit 1
  read -r steer2 || exit 1
  emit '{"type":"result","subtype":"success","result":"story","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-sup"}'
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-sup"}'
  emit '{"type":"result","subtype":"error_during_execution","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-sup"}'
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-sup"}'
  emit "$steer2"
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"answered-both"}}}'
  emit '{"type":"result","subtype":"success","result":"answered-both","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-sup"}'
  ;;

*scenario:absorbed-steer*)
  # A steer whose replay never comes: the turn end must not be held forever.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-abs"}'
  read -r steer || exit 1
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"absorbed"}}}'
  emit '{"type":"result","subtype":"success","result":"absorbed","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-abs"}'
  exec sleep 30
  ;;

*scenario:interrupt*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-int"}'
  # Wedge without reading stdin — forces the SIGTERM escalation path.
  exec sleep 30
  ;;

*scenario:resume-check*)
  # `--resume` of a session the CLI has no conversation for: live 2.1.309
  # answers with a result and no init, says so on stderr, and exits 1.
  resumed=none
  for arg in "$@"; do
    case "$arg" in --resume=*) resumed="${arg#--resume=}" ;; esac
  done
  case "$resumed" in
  stale-*)
    echo "No conversation found with session ID: $resumed" >&2
    emit "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"num_turns\":0,\"session_id\":\"$resumed\",\"errors\":[\"No conversation found with session ID: $resumed\"]}"
    exit 1
    ;;
  stderr-only-*)
    echo "No conversation found with session ID: $resumed" >&2
    exit 1
    ;;
  none) sid=sess-fresh ;;
  *) sid=$resumed ;;
  esac
  emit "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"claude-fable-5\",\"tools\":[],\"cwd\":\"/tmp\",\"session_id\":\"$sid\"}"
  emit "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"resumed=$resumed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"$sid\"}"
  ;;

*scenario:unknown-control*)
  # A control request this host does not serve must still be answered.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-uc"}'
  emit '{"type":"control_request","request_id":"cr-x","request":{"subtype":"hook_callback","callback_id":"h1"}}'
  read -r reply || exit 1
  result=unanswered
  case "$reply" in
  *'"request_id":"cr-x"'*) case "$reply" in *'"subtype":"error"'*) result=answered ;; esac ;;
  esac
  emit "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$result\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-uc\"}"
  ;;

*scenario:grandchild*)
  # An MCP server or tool the CLI started, still running when it exits.
  sleep 300 </dev/null >/dev/null 2>&1 &
  printf '%s' "$!" > "$RENO_TEST_PIDFILE"
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-gc"}'
  emit '{"type":"result","subtype":"success","result":"done","usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-gc"}'
  ;;

*scenario:unreadable-result*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-ur"}'
  emit '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"half \ud83d"}}}'
  emit '{"type":"result","subtype":"success","result":"x","usage":{"input_tokens":"lots"},"session_id":"sess-ur"}'
  # Stays up like the real CLI between turns: only the result ends the turn.
  read -r _ || exit 0
  ;;

*scenario:error*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-err"}'
  # Terse assistant-level error code with no content.
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[]},"error":"rate_limit"}'
  # Hard-rejected claude.ai usage window.
  emit '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}'
  # Result error with an EMPTY errors array: needs fallback wording.
  emit '{"type":"result","subtype":"error_max_turns","errors":[],"usage":{"input_tokens":1,"output_tokens":2},"session_id":"sess-err"}'
  ;;

*scenario:approval-subjects*)
  # One permission request per tool class. The result says how each was
  # answered, and what a deny said. Frame shapes as live 2.1.228 sends them.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-subj"}'
  answer() {
    read -r resp || exit 1
    case "$resp" in
    *"\"request_id\":\"$1\""*'"behavior":"allow"'*) printf allow ;;
    *"\"request_id\":\"$1\""*'"behavior":"deny"'*)
      printf 'deny:%s' "$(printf '%s\n' "$resp" | sed 's/.*"message":"\([^"]*\)".*/\1/')"
      ;;
    *) exit 1 ;;
    esac
  }
  emit '{"type":"control_request","request_id":"cr-bash","request":{"subtype":"can_use_tool","tool_name":"Bash","display_name":"Bash","description":"Run the tests","input":{"command":"cargo test --workspace","description":"Run the tests"}}}'
  bash=$(answer cr-bash) || exit 1
  emit '{"type":"control_request","request_id":"cr-edit","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"/work/src/lib.rs","old_string":"a","new_string":"b"}}}'
  edit=$(answer cr-edit) || exit 1
  emit '{"type":"control_request","request_id":"cr-mcp","request":{"subtype":"can_use_tool","tool_name":"mcp__x__y","input":{"q":"bug"}}}'
  mcp=$(answer cr-mcp) || exit 1
  emit '{"type":"control_request","request_id":"cr-net","request":{"subtype":"can_use_tool","tool_name":"WebFetch","input":{"url":"https://example.com","prompt":"summarise"}}}'
  net=$(answer cr-net) || exit 1
  emit '{"type":"control_request","request_id":"cr-read","request":{"subtype":"can_use_tool","tool_name":"Read","input":{"file_path":"/etc/hosts"}}}'
  read_=$(answer cr-read) || exit 1
  emit '{"type":"control_request","request_id":"cr-other","request":{"subtype":"can_use_tool","tool_name":"TodoWrite","input":{"todos":[]}}}'
  other=$(answer cr-other) || exit 1
  emit "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"bash=$bash edit=$edit mcp=$mcp net=$net read=$read_ other=$other\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-subj\"}"
  ;;

*scenario:one-approval*)
  # A single command the run must get approved: the kernel's end-to-end
  # scenario. Ends `ran` when allowed, `refused` when denied.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-one"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"about to run a command. "}}}'
  emit '{"type":"control_request","request_id":"cr-one","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm -rf build"}}}'
  read -r resp || exit 1
  case "$resp" in
  *'"request_id":"cr-one"'*'"behavior":"allow"'*) result=ran ;;
  *'"request_id":"cr-one"'*'"behavior":"deny"'*) result=refused ;;
  *) exit 1 ;;
  esac
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"tool-one","name":"Bash","input":{"command":"rm -rf build"}}]}}'
  emit '{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"tool-one","is_error":false}]}}'
  emit "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$result\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-one\"}"
  ;;

*scenario:hold-approval*)
  # A permission request nobody answers: the answer that reaches stdin is
  # recorded in $RENO_TEST_RECORD, then the CLI exits.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-hold"}'
  emit '{"type":"control_request","request_id":"cr-hold","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}'
  while read -r line; do
    case "$line" in
    *'"request_id":"cr-hold"'*)
      printf '%s\n' "$line" > "$RENO_TEST_RECORD"
      exit 0
      ;;
    esac
  done
  ;;

*scenario:request-after-cancel*)
  # A permission request that reaches the driver only after the run was cancelled: whatever
  # answers it is recorded in $RENO_TEST_RECORD, then the CLI exits.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-late"}'
  while read -r line; do
    case "$line" in *'"subtype":"interrupt"'*) break ;; esac
  done
  emit '{"type":"control_request","request_id":"cr-late","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}'
  while read -r line; do
    case "$line" in
    *'"request_id":"cr-late"'*)
      printf '%s\n' "$line" > "$RENO_TEST_RECORD"
      exit 0
      ;;
    esac
  done
  ;;

*scenario:crash-mid-turn*)
  # The CLI dies during its turn, before any result.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-crash"}'
  emit '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"working"}}}'
  # stdout closes before the last stderr line is written.
  exec 1>&-
  sleep 0.05
  echo "FATAL: heap out of memory" >&2
  echo "FATAL: heap out of memory" >&2
  exit 3
  ;;

*scenario:conf-mcp*)
  # Conformance: the inline `--mcp-config` carries the injected server.
  cfg=none
  prev=
  for arg in "$@"; do
    [ "$prev" = "--mcp-config" ] && cfg=$arg
    prev=$arg
  done
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-mcp"}'
  case "$cfg" in
  *'"conf-server"'*'"/opt/conf/server"'*) result=mcp:conf-server ;;
  *) result="mcp:missing:$(printf '%s' "$cfg" | tr -d '"\\')" ;;
  esac
  emit "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$result\",\"errors\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"session_id\":\"sess-mcp\"}"
  ;;

*scenario:conf-unknown*)
  # Conformance: a frame type from a CLI newer than the driver.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-unk"}'
  emit '{"type":"zz_future_frame","payload":{"n":1}}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"after the unknown frame"}}}'
  emit '{"type":"result","subtype":"success","result":"unknown-done","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-unk"}'
  ;;

*)
  emit '{"type":"result","subtype":"error_during_execution","errors":["unknown scenario"],"usage":{"input_tokens":0,"output_tokens":0},"session_id":"sess-x"}'
  ;;
esac
