# ROAD-33 transcript

<!--
The operator's intent for one road, written at /idea while they are at the keyboard.
`/shape --auto` reads every slot as their standing word; agents decide everything else.
At most 30 non-blank lines: a file path, a table name or an API shape belongs in the spec,
not here.
-->

**Outcome:** The owner reads, finds, selects and copies anything in a conversation of any length without a dropped frame, and replies show typeset math, diagrams, images and the visuals an agent posts. Everything is designed on the mocked daemon and kept as real code.

**Examples:**
1. Jayden opens the thread "fix the ACP v2 handshake", which has 2,000 messages. He opens every "Worked for" fold and scrolls from top to bottom without a stutter. He presses ⌘F, types `supervisor.rs`, sees "1 of 7" and presses Enter. Turn 3's fold opens at the `cargo test` step that touched that file. Esc closes the search, and that fold closes again.
2. Jayden drags from the middle of his prompt to the middle of Codex's reply below it, presses ⌘C, and pastes exactly that text in order. In the same reply, `$$\int_0^1 x^2\,dx = \tfrac13$$` shows as typeset math, a Mermaid flowchart shows as a diagram, and the Copy button on a code block copies only the code.

**Must keep:** the "Worked for" folds, tool groups, Copy on a whole message, the prompt minimap, the "Thinking" line, subagent chips, following the tail while a run streams, and every existing transcript lock test.
**Off limits:** scripts that an agent writes and Reno runs (a road of their own); the real daemon side of agent-posted visuals (after MILE-10); changing how the daemon keeps run journals.
**Decided:** everything the reference agent panel ships, including diagrams and images, plus typeset math and the static images and diagrams an agent posts; selection spans messages; ⌘F searches everything (prompts, replies, interim text, reasoning, commands, tool output), going to a match inside a closed fold opens it, and closing the search closes again the folds it opened; frame time p95 ≤ 16.7 ms on 2,000 messages with every fold open.
**Delegated:** how rows are split to stay fast; the math and diagram renderers; streaming pace; the look of each variant before the owner's pick.
**Ask me:** None
**UI:** new
