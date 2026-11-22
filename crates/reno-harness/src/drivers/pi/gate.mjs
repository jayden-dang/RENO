// Reno's gate for one Pi attempt, loaded with `--extension` after every other extension. This
// stub blocks every tool call and registers the `reno-gate` command that tells Reno the gate is
// loaded.
export default function (pi) {
  pi.registerCommand("reno-gate", {
    description: "Reno's gate on tool calls",
    handler: async () => {},
  });
  pi.on("tool_call", async () => ({ block: true, reason: "Reno's gate is not connected" }));
}
