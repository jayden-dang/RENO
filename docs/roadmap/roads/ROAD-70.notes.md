# ROAD-70 change-provenance — notes

Every changed range in a run's worktree is traced to the tool call and the message that wrote it,
so "why is this line here" opens the transcript at that call. Reno already journals each tool call
and gives each write run its own worktree. This item joins the two: diff hunks are attributed to
tool calls, both when the harness edits through a tool and when it edits the disk directly. The
attribution must survive later edits and rebases. It feeds the panel's Changes view and a future
review thread (MILE-9).
