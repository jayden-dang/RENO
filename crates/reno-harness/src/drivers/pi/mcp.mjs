// Per-run Reno MCP bridge for Pi. Loaded with `--extension <file>` from a scratch directory that
// belongs to the run: no Pi setting, package or `mcp.json` is written or read. The servers of the
// run are described by the JSON file named in RENO_PI_MCP: [{ name, command, args, env }, ...]
// (Reno's own server first, then a plugin's).
//
// Each tool of a server becomes a Pi tool named like Pi's own MCP tools, `mcp__<server>__<tool>`
// (every character other than letters, digits and `_` replaced by `_`, shortened to the 64
// characters model APIs allow with a hash suffix), so Reno classifies a call the same way
// whichever side connected the server.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { createInterface } from "node:readline";

const MAX_NAME = 64;
// A Reno tool may wait for an hour (a delegated chat), plus margin for the reply.
const CALL_TIMEOUT_MS = (3600 + 120) * 1000;
const LIST_TIMEOUT_MS = 15_000;

const clean = (text) => String(text).replace(/[^A-Za-z0-9_]/g, "_");

export function toolName(server, tool) {
  const name = `mcp__${clean(server)}__${clean(tool)}`;
  if (name.length <= MAX_NAME) return name;
  const hash = createHash("sha256").update(`${server}\0${tool}`).digest("hex").slice(0, 8);
  return `${name.slice(0, MAX_NAME - 9)}_${hash}`;
}

// One connection to one stdio MCP server.
function connection(pi, config) {
  let child, lines, nextId = 0;
  const pending = new Map();
  function fail(error) {
    for (const request of [...pending.values()]) request.reject(error);
  }
  function close() {
    fail(new Error(`Reno MCP ${config.name} connection closed`));
    lines?.close();
    child?.kill();
    child = undefined;
  }
  function rpc(method, params, signal) {
    if (!child?.stdin.writable) return Promise.reject(new Error(`Reno MCP ${config.name} is not connected`));
    if (signal?.aborted) return Promise.reject(new Error(`Reno MCP ${config.name} request cancelled`));
    return new Promise((resolve, reject) => {
      const id = ++nextId;
      const cleanup = () => {
        clearTimeout(timer);
        signal?.removeEventListener("abort", abort);
        pending.delete(id);
      };
      const abort = () => {
        child?.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/cancelled", params: { requestId: id } }) + "\n");
        cleanup();
        reject(new Error(`Reno MCP ${config.name} request cancelled`));
      };
      const timer = setTimeout(() => {
        cleanup();
        reject(new Error(`Reno MCP ${config.name} ${method} timed out`));
      }, method === "tools/call" ? CALL_TIMEOUT_MS : LIST_TIMEOUT_MS);
      pending.set(id, {
        resolve: (value) => { cleanup(); resolve(value); },
        reject: (error) => { cleanup(); reject(error); },
      });
      signal?.addEventListener("abort", abort, { once: true });
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n", (error) => {
        if (error) pending.get(id)?.reject(error);
      });
    });
  }
  async function start() {
    close();
    child = spawn(config.command, config.args ?? [], {
      env: { ...process.env, ...config.env }, stdio: ["pipe", "pipe", "inherit"],
    });
    const owned = child;
    const failed = (error) => { if (child === owned) fail(error); };
    child.on("error", failed);
    child.on("exit", () => failed(new Error(`Reno MCP ${config.name} server exited`)));
    child.stdin.on("error", failed);
    lines = createInterface({ input: child.stdout });
    lines.on("line", (line) => {
      let message;
      try { message = JSON.parse(line); } catch { return; }
      const request = pending.get(message.id);
      if (!request) return;
      if (message.error) request.reject(new Error(message.error.message));
      else request.resolve(message.result);
    });
    try {
      await rpc("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "reno-pi", version: "1" } });
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
      const { tools } = await rpc("tools/list", {});
      for (const tool of tools) {
        pi.registerTool({
          name: toolName(config.name, tool.name), label: `Reno: ${tool.name}`,
          description: tool.description, parameters: tool.inputSchema,
          async execute(_id, args, signal) {
            const result = await rpc("tools/call", { name: tool.name, arguments: args }, signal);
            if (result.isError) throw new Error(result.content?.filter((c) => c.type === "text").map((c) => c.text).join("\n") || `Reno tool ${tool.name} failed`);
            return { content: result.content, details: result.structuredContent ?? {} };
          },
        });
      }
    } catch (error) {
      close();
      throw error;
    }
  }
  return { start, close };
}

export default function (pi) {
  const servers = JSON.parse(readFileSync(process.env.RENO_PI_MCP, "utf8")).map((config) => connection(pi, config));
  pi.on("session_start", async () => {
    // One server failing to start leaves the others' tools registered.
    const errors = [];
    for (const server of servers) {
      try { await server.start(); } catch (error) { errors.push(error); }
    }
    if (errors.length) throw errors[0];
  });
  pi.on("session_shutdown", () => servers.forEach((server) => server.close()));
  process.once("exit", () => servers.forEach((server) => server.close()));
}
