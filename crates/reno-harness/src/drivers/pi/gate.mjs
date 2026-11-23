// Reno's gate for one Pi attempt, loaded with `--extension` after every other extension. Every
// tool call Pi is about to run is sent to Reno over the run's private Unix socket and waits there
// for Reno's decision; anything but an explicit allow blocks the call.
//
// Environment: RENO_GATE_SOCKET (the socket path) and RENO_GATE_NONCE (proves this is the run's
// own gate). Lines, one JSON object each:
//   gate -> Reno: {"t":"hello","nonce"} once, at startup;
//                 {"t":"call","id","toolName","toolCallId","parentToolCallId","input"}
//   Reno -> gate: {"t":"decision","id","decision":"allow"|"deny","reason"}
//
// The gate has no timer: Reno answers every call (an expiry is a deny, a cancel denies all that
// are open), so it waits for the answer or for the socket to close. It never reconnects: once
// the connection is lost, or anything unexpected happens, every call blocks until Pi exits.
import { connect } from "node:net";

export const MAX_OUTSTANDING = 16;
export const MAX_WAITING = 256;
// The longest reply Reno may send without a newline. It is checked once per received chunk, so the
// line that trips it is one chunk past 1 MiB.
const MAX_LINE = 1 << 20;

const block = (reason) => ({ block: true, reason });
const cannotJudge = (why) => `Reno could not judge this call: ${why}.`;

export default function (pi) {
  let socket;
  let ready;
  let lostReason = cannotJudge("the gate is not connected");
  let live = false;
  let connected = false;
  let nextId = 0;
  const outstanding = new Map();
  const waiting = [];

  function lose(why) {
    if (!live) return;
    live = false;
    lostReason = cannotJudge(why);
    socket?.destroy();
    for (const call of [...outstanding.values(), ...waiting.splice(0)]) call.resolve(block(lostReason));
    outstanding.clear();
  }

  function send(call) {
    const id = ++nextId;
    const { event } = call;
    let line;
    try {
      line = JSON.stringify({
        t: "call",
        id,
        toolName: event.toolName,
        toolCallId: event.toolCallId,
        parentToolCallId: event.parentToolCallId ?? null,
        input: event.input,
      });
    } catch (error) {
      lose(`the call could not be described (${error.message})`);
      call.resolve(block(lostReason));
      return;
    }
    outstanding.set(id, call);
    socket.write(line + "\n");
  }

  function pump() {
    while (live && outstanding.size < MAX_OUTSTANDING && waiting.length) send(waiting.shift());
  }

  function decided(message) {
    const call = message && typeof message === "object" ? outstanding.get(message.id) : undefined;
    if (message?.t !== "decision" || !call) throw new Error("an unexpected line came from Reno");
    if (message.decision !== "allow" && message.decision !== "deny") throw new Error("an unknown decision came from Reno");
    outstanding.delete(message.id);
    if (message.decision === "allow") call.resolve(undefined);
    else call.resolve(block(typeof message.reason === "string" && message.reason ? message.reason : "Reno did not allow this call."));
    pump();
  }

  function onData(state) {
    return (chunk) => {
      try {
        state.buffer += chunk;
        let at;
        while (live && (at = state.buffer.indexOf("\n")) >= 0) {
          const line = state.buffer.slice(0, at);
          state.buffer = state.buffer.slice(at + 1);
          decided(JSON.parse(line));
        }
        if (live && state.buffer.length > MAX_LINE) throw new Error("a line from Reno is too long");
      } catch (error) {
        lose(`its reply was malformed (${error.message})`);
      }
    };
  }

  function open() {
    const path = process.env.RENO_GATE_SOCKET;
    const nonce = process.env.RENO_GATE_NONCE;
    if (!path || !nonce) {
      lostReason = cannotJudge("the gate has no socket to reach Reno");
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      const conn = connect(path);
      conn.setEncoding("utf8");
      socket = conn;
      conn.once("connect", () => {
        live = true;
        connected = true;
        conn.write(JSON.stringify({ t: "hello", nonce }) + "\n");
        resolve();
      });
      conn.on("data", onData({ buffer: "" }));
      conn.on("error", () => {});
      conn.on("close", () => {
        if (live) lose("the connection to Reno closed");
        else if (!connected) lostReason = cannotJudge("Reno's socket could not be reached");
        resolve();
      });
    });
  }

  function judge(event) {
    return new Promise((resolve) => {
      const call = { event, resolve };
      if (outstanding.size < MAX_OUTSTANDING) send(call);
      else if (waiting.length < MAX_WAITING) waiting.push(call);
      else resolve(block("too many calls are waiting for Reno"));
    });
  }

  pi.registerCommand("reno-gate", {
    description: "Reno's gate on tool calls",
    handler: async () => {},
  });

  pi.on("session_start", async (event) => {
    if (event?.reason !== "startup" || ready) return;
    ready = open();
    await ready;
  });

  pi.on("tool_call", async (event) => {
    try {
      await ready;
      if (!live) return block(lostReason);
      return await judge(event);
    } catch (error) {
      lose(`the gate failed (${error?.message ?? error})`);
      return block(lostReason);
    }
  });
}
