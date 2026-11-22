// Exercises Reno's Pi gate extension (src/drivers/pi/gate.mjs) against a scripted Unix socket that
// plays the driver. Run as `node pi-gate-test.mjs <path to gate.mjs>`. Pi is not involved: the gate
// is an extension function, driven with the calls Pi makes of it (`on`, `registerCommand`).
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { Socket, createServer } from "node:net";
import { join } from "node:path";

// The gate must not measure time at all: trap every way it could, for the whole run.
const clock = [];
for (const name of ["setTimeout", "setInterval"]) {
  const real = globalThis[name];
  globalThis[name] = (...args) => (clock.push(name), real(...args));
}
const realNow = Date.now;
Date.now = () => (clock.push("Date.now"), realNow());
const realSocketTimeout = Socket.prototype.setTimeout;
Socket.prototype.setTimeout = function (...args) {
  clock.push("socket.setTimeout");
  return realSocketTimeout.apply(this, args);
};
assert.doesNotMatch(readFileSync(process.argv[2], "utf8"), /setTimeout|setInterval|Date\.now|AbortSignal\.timeout/);

const { default: extension, MAX_OUTSTANDING, MAX_WAITING } = await import(process.argv[2]);
// A short directory, so the socket path stays within sun_path whatever TMPDIR is.
const dir = mkdtempSync("/tmp/rg-");
const NONCE = "nonce-1234";
let sockets = 0;

// The driver side of one connection: lines received in order, and what was sent back.
function listen() {
  const path = join(dir, `g${++sockets}.sock`);
  const peer = { path, received: [], waiters: [], conn: undefined, closed: false };
  const accepted = new Promise((resolve) => {
    peer.server = createServer((conn) => {
      peer.conn = conn;
      let buffer = "";
      conn.on("data", (chunk) => {
        buffer += chunk;
        let at;
        while ((at = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, at);
          buffer = buffer.slice(at + 1);
          const waiter = peer.waiters.shift();
          if (waiter) waiter(line);
          else peer.received.push(line);
        }
      });
      conn.on("close", () => (peer.closed = true));
      conn.on("error", () => {});
      resolve();
    });
  });
  peer.server.listen(path);
  peer.accepted = accepted;
  peer.line = () => (peer.received.length ? Promise.resolve(peer.received.shift()) : new Promise((resolve) => peer.waiters.push(resolve)));
  peer.next = async () => JSON.parse(await peer.line());
  peer.send = (object) => peer.conn.write(JSON.stringify(object) + "\n");
  peer.raw = (text) => peer.conn.write(text);
  peer.decide = (id, decision, reason = null) => peer.send({ t: "decision", id, decision, reason });
  return peer;
}

// Loads the gate as Pi would, with a fresh stub API, and starts its session.
async function load(path, { start = true } = {}) {
  process.env.RENO_GATE_SOCKET = path;
  process.env.RENO_GATE_NONCE = NONCE;
  const handlers = {}, commands = {};
  extension({ on: (event, fn) => (handlers[event] = fn), registerCommand: (name, command) => (commands[name] = command) });
  if (start) await handlers.session_start({ type: "session_start", reason: "startup" });
  return {
    handlers,
    commands,
    call: (toolName, input, extra = {}) => handlers.tool_call({ type: "tool_call", toolName, toolCallId: `c-${toolName}`, input, ...extra }),
  };
}

const settled = async (promise) => {
  const nothing = Symbol();
  return (await Promise.race([promise, new Promise((resolve) => setImmediate(() => resolve(nothing)))])) === nothing ? "pending" : "settled";
};

const turns = async (count) => {
  for (let i = 0; i < count; i++) await new Promise((resolve) => setImmediate(resolve));
};

async function connected() {
  const peer = listen();
  const gate = await load(peer.path);
  await peer.accepted;
  return { peer, gate };
}

// The gate registers its command, with an empty handler, so Reno can tell it is loaded.
{
  const { gate } = await connected();
  assert.ok(gate.commands["reno-gate"], "reno-gate is registered");
  assert.equal(await gate.commands["reno-gate"].handler(), undefined);
}

// Hello carries the nonce, once, at startup only.
{
  const { peer, gate } = await connected();
  assert.deepEqual(await peer.next(), { t: "hello", nonce: NONCE });
  await gate.handlers.session_start({ type: "session_start", reason: "reload" });
  const first = gate.call("bash", { command: "ls" });
  const sent = await peer.next();
  assert.equal(sent.t, "call", "a reload does not say hello again");
  peer.decide(sent.id, "allow");
  await first;
}

// Every call is sent, nested ones with their parent. Allow returns undefined and leaves input alone.
{
  const { peer, gate } = await connected();
  await peer.next();
  const input = { command: "echo hi", nested: { a: [1, 2] } };
  const before = structuredClone(input);
  const top = gate.call("bash", input);
  const sent = await peer.next();
  assert.deepEqual(sent, { t: "call", id: sent.id, toolName: "bash", toolCallId: "c-bash", parentToolCallId: null, input: before });
  assert.equal(typeof sent.id, "number");
  const nested = gate.call("read", { path: "a.txt" }, { toolCallId: "c-nested", parentToolCallId: "c-bash" });
  const sentNested = await peer.next();
  assert.equal(sentNested.toolCallId, "c-nested");
  assert.equal(sentNested.parentToolCallId, "c-bash");
  assert.notEqual(sentNested.id, sent.id);
  // Answered out of order.
  peer.decide(sentNested.id, "allow");
  assert.equal(await nested, undefined);
  peer.decide(sent.id, "allow");
  assert.equal(await top, undefined);
  assert.deepEqual(input, before, "the hook leaves input untouched");
}

// An MCP tool and an extension's own tool are sent like any other.
{
  const { peer, gate } = await connected();
  await peer.next();
  for (const name of ["mcp__reno__whoami", "some_extension_tool"]) {
    const result = gate.call(name, {});
    const sent = await peer.next();
    assert.equal(sent.toolName, name);
    peer.decide(sent.id, "allow");
    assert.equal(await result, undefined);
  }
}

// Deny blocks with the driver's reason.
{
  const { peer, gate } = await connected();
  await peer.next();
  const result = gate.call("write", { path: ".reno/x", content: "" });
  const sent = await peer.next();
  peer.decide(sent.id, "deny", "The policy keeps .reno to Reno.");
  assert.deepEqual(await result, { block: true, reason: "The policy keeps .reno to Reno." });
  // A deny with no reason still blocks, and says why in general terms.
  const next = gate.call("bash", { command: "rm -rf x" });
  const second = await peer.next();
  peer.decide(second.id, "deny");
  const blocked = await next;
  assert.equal(blocked.block, true);
  assert.ok(blocked.reason.length > 0);
}

// No connection: before the session starts, and with nothing listening.
{
  const never = await load(join(dir, "nobody.sock"), { start: false });
  const early = await never.call("bash", { command: "ls" });
  assert.equal(early.block, true);
  assert.match(early.reason, /not connected/);

  const absent = await load(join(dir, "nobody.sock"));
  const result = await absent.call("bash", { command: "ls" });
  assert.equal(result.block, true);
  assert.match(result.reason, /could not be reached/);
}

// A closed socket blocks the call in flight and every call after it.
{
  const { peer, gate } = await connected();
  await peer.next();
  const open = gate.call("bash", { command: "ls" });
  await peer.next();
  peer.conn.destroy();
  const blocked = await open;
  assert.equal(blocked.block, true);
  assert.match(blocked.reason, /the connection to Reno closed/);
  const after = await gate.call("bash", { command: "ls" });
  assert.deepEqual(after, blocked, "the reason survives the socket closing");
}

// A malformed line blocks the call in flight and every call after it.
for (const garbage of ["this is not json\n", "[1,2]\n", `{"t":"decision","id":9999,"decision":"allow"}\n`, `{"t":"decision","id":1,"decision":"maybe"}\n`, "x".repeat(2 << 20)]) {
  const { peer, gate } = await connected();
  await peer.next();
  const open = gate.call("bash", { command: "ls" });
  await peer.next();
  peer.raw(garbage);
  const blocked = await open;
  assert.equal(blocked.block, true, garbage);
  assert.match(blocked.reason, garbage.length > 1000 ? /too long/ : /malformed/);
  await turns(20);
  assert.equal(peer.closed, true, "the gate drops the connection it cannot trust");
  const after = await gate.call("bash", { command: "ls" });
  assert.deepEqual(after, blocked, garbage.slice(0, 40));
}

// A handler that throws blocks the call and every call after it, even one the driver would allow.
{
  const { peer, gate } = await connected();
  await peer.next();
  const circular = {};
  circular.self = circular;
  const thrown = await gate.call("bash", circular);
  assert.equal(thrown.block, true);
  assert.match(thrown.reason, /could not be described/);
  const result = await gate.call("bash", { command: "ls" });
  assert.deepEqual(result, thrown);
}

// There is no timer of its own: a reply withheld across 1000 turns of the event loop leaves the
// hook pending, and the late allow then lets the call through.
{
  const { peer, gate } = await connected();
  await peer.next();
  const result = gate.call("bash", { command: "sleep 1" });
  const sent = await peer.next();
  await turns(1000);
  assert.equal(await settled(result), "pending");
  peer.decide(sent.id, "allow");
  assert.equal(await result, undefined);
}

// At most MAX_OUTSTANDING calls are sent unanswered: the next one waits inside the gate, and goes
// out, in order, as answers arrive.
{
  assert.equal(MAX_OUTSTANDING, 16);
  assert.equal(MAX_WAITING, 256);
  const { peer, gate } = await connected();
  await peer.next();
  const results = [], sent = [];
  for (let i = 0; i < MAX_OUTSTANDING; i++) {
    results.push(gate.call("bash", { command: `c${i}` }));
    sent.push(await peer.next());
  }
  const waiting = gate.call("bash", { command: "c16" });
  await turns(50);
  assert.equal(peer.received.length, 0, "the 17th call is not sent");
  assert.equal(await settled(waiting), "pending");
  peer.decide(sent[3].id, "allow");
  assert.equal(await results[3], undefined);
  const late = await peer.next();
  assert.equal(late.input.command, "c16", "the waiting call goes out when a slot frees");
  peer.decide(late.id, "deny", "no");
  assert.deepEqual(await waiting, { block: true, reason: "no" });
}

// Past the wait queue, a call is blocked at once; the queue itself is not lost.
{
  const { peer, gate } = await connected();
  await peer.next();
  const results = [], sent = [];
  for (let i = 0; i < MAX_OUTSTANDING; i++) {
    results.push(gate.call("bash", { command: `c${i}` }));
    sent.push(await peer.next());
  }
  const waiting = [];
  for (let i = 0; i < MAX_WAITING; i++) waiting.push(gate.call("bash", { command: `w${i}` }));
  const over = await gate.call("bash", { command: "over" });
  assert.deepEqual(over, { block: true, reason: "too many calls are waiting for Reno" });
  assert.equal(await settled(waiting[0]), "pending");
  peer.decide(sent[0].id, "allow");
  const first = await peer.next();
  assert.equal(first.input.command, "w0");
  peer.decide(first.id, "allow");
  assert.equal(await waiting[0], undefined);
  // The connection is still live after the overflow.
  assert.equal(peer.closed, false);
}

assert.deepEqual(clock, [], "the gate never reads a clock or sets a timer");
rmSync(dir, { recursive: true, force: true });
process.exit(0);
