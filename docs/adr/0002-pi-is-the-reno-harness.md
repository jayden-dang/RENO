# ADR-0002: Pi is the Reno harness

Status: Accepted
Date: 2026-11-22

ADR-0001 left the choice of the Reno harness to a spike between Pi and dsh, judged first on ARCH-18 (every tool call passes Reno's policy before it runs, fail-closed). Pi is the Reno harness: an extension on its `tool_call` hook reached Reno for every call in the spike (shell, file edits, MCP, parallel and nested calls), blocked every call when the gate or the daemon failed, could rewrite a call's arguments, and cost about 0.1–0.3 ms per call, 0.19 s startup and 114–136 MB. dsh was rejected because its subagents run under a fixed no-ask policy that Reno cannot answer, a missing gate plugin lets tools run ungated, approval requests never time out and carry no arguments, and it needs 1.0–1.3 s, about 343 MB and a 1.35 GB install; its goal-driven continuation is the part worth keeping, and Reno builds it in the kernel instead.
