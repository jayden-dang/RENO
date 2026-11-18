// Exercises Reno's Pi MCP bridge (src/drivers/pi/mcp.mjs) against a real stdio MCP peer: this same
// file, started with `server`. Run as `node pi-mcp-test.mjs <path to mcp.mjs>`.
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";

if (process.argv[2] === "server") {
  createInterface({ input: process.stdin }).on("line", (line) => {
    const msg = JSON.parse(line);
    let result;
    if (msg.method === "initialize") result = { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "test", version: "1" } };
    else if (msg.method === "tools/list") {
      result = { tools: ["whoami", "create_chat", "read_chat", "send_message", "wait_for_turn", "odd.name-here"].map((name) => ({ name, description: "Reno delegation", inputSchema: { type: "object", properties: { mode: { type: "string" } } } })) };
    } else if (msg.method === "tools/call") {
      if (msg.params.arguments.mode === "hang") return;
      if (msg.params.arguments.mode === "crash") process.exit(1);
      result = { content: [{ type: "text", text: process.env.RENO_CHAT_ID }], isError: msg.params.arguments.mode === "error" };
    } else return;
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result }) + "\n");
  });
} else {
  const bridge = process.argv[2];
  const { default: extension, toolName } = await import(bridge);

  // Names follow Pi's own MCP client: mcp__<server>__<tool>, every other character a `_`, at most 64.
  assert.equal(toolName("reno", "whoami"), "mcp__reno__whoami");
  assert.equal(toolName("plugin-focus", "odd.name-here"), "mcp__plugin_focus__odd_name_here");
  const long = toolName("reno", "t".repeat(80));
  assert.equal(long.length, 64);
  assert.match(long, /^mcp__reno__t+_[0-9a-f]{8}$/);
  assert.notEqual(long, toolName("reno", "t".repeat(81)));

  const dir = mkdtempSync(join(tmpdir(), "reno-pi-mcp-test-"));
  for (const chat of ["first", "second"]) {
    // Two servers in one config: each keeps its own tools and its own environment.
    const servers = [
      { name: "reno", command: process.execPath, args: [process.argv[1], "server"], env: { RENO_CHAT_ID: chat } },
      { name: "plugin-focus", command: process.execPath, args: [process.argv[1], "server"], env: { RENO_CHAT_ID: `${chat}-plugin` } },
    ];
    const config = join(dir, `${chat}.json`);
    writeFileSync(config, JSON.stringify(servers));
    process.env.RENO_PI_MCP = config;
    const handlers = {}, tools = {};
    extension({ on: (event, fn) => (handlers[event] = fn), registerTool: (tool) => (tools[tool.name] = tool) });
    try {
      await handlers.session_start();
      for (const name of ["create_chat", "read_chat", "send_message", "wait_for_turn"]) {
        assert.equal((await tools[`mcp__reno__${name}`].execute("delegation", {})).content[0].text, chat);
      }
      assert.equal((await tools["mcp__plugin_focus__whoami"].execute("call", {})).content[0].text, `${chat}-plugin`);
      assert.ok(tools["mcp__reno__odd_name_here"], "a tool name with other characters is cleaned");
      const tool = tools.mcp__reno__whoami;
      assert.equal(tool.parameters.type, "object");
      assert.equal((await tool.execute("call", {})).content[0].text, chat);
      await assert.rejects(tool.execute("call", { mode: "error" }), new RegExp(chat));
      const controller = new AbortController();
      const waiting = tool.execute("call", { mode: "hang" }, controller.signal);
      controller.abort();
      await assert.rejects(waiting, /cancelled/);
      await assert.rejects(tool.execute("call", { mode: "crash" }), /exited/);
      assert.equal(handlers.message_end, undefined, "native RPC reports provider failures directly");
      // Restarting the session must not let an old child's exit fail new RPCs.
      await handlers.session_start();
      assert.equal((await tools.mcp__reno__whoami.execute("call", {})).content[0].text, chat);
    } finally {
      handlers.session_shutdown();
    }
    await assert.rejects(tools.mcp__reno__whoami.execute("call", {}), /not connected/);
  }

  // One server that cannot start does not take the others' tools with it.
  const tools = {}, handlers = {};
  const broken = [
    { name: "gone", command: "/nonexistent/reno-mcp-server", args: [], env: {} },
    { name: "reno", command: process.execPath, args: [process.argv[1], "server"], env: { RENO_CHAT_ID: "x" } },
  ];
  const config = join(dir, "broken.json");
  writeFileSync(config, JSON.stringify(broken));
  process.env.RENO_PI_MCP = config;
  extension({ on: (event, fn) => (handlers[event] = fn), registerTool: (tool) => (tools[tool.name] = tool) });
  await assert.rejects(handlers.session_start());
  assert.ok(tools.mcp__reno__whoami, "the healthy server's tools are registered");
  handlers.session_shutdown();
}
