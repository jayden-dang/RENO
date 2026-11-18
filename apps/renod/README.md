# renod

The Reno daemon. It opens the store, builds the executors, starts the kernel and serves the
client protocol on a Unix socket. The CLI (`reno`) and the app talk to it through `reno-client`.

```
renod [--data-dir DIR] [--socket PATH]
```

Normally you do not run it by hand: `reno daemon start` starts it detached and waits until it
answers, `reno daemon stop` stops it, and `reno daemon install` hands it to a per-user LaunchAgent
(`dev.reno.renod`) that starts it at login and restarts it after a crash. The app starts one when
it finds none. SIGTERM and SIGINT (what launchd sends) take the same path as `daemon.shutdown`:
stop accepting, interrupt the live runs, end their process groups, remove the socket, release the
lock, exit 0.

## Files

| Path | What |
|---|---|
| `<data-dir>/` | mode 0700. macOS: `~/Library/Application Support/Reno`. Elsewhere: `$XDG_DATA_HOME/reno`, or `~/.local/share/reno` |
| `engine.lock` | an exclusive `flock`, held for the life of the process, taken before anything else is opened. A second `renod` on the same data directory exits non-zero and says who holds it |
| `reno.sock` | the socket, mode 0600 (`--socket` moves it) |
| `reno.db`, `journals/` | the store |
| `logs/renod.log` | JSON lines through a non-blocking writer, rotated by size: at 10 MiB it becomes `renod.log.1`, then `.2`; three files at most, so 30 MiB |
| `logs/renod.stderr` | where a detached daemon's own stderr goes (set by the launcher): early errors and what no logger caught. Emptied at start when it is over 1 MiB |
| `workspace/` | where a task that names no working directory runs |
| `worktrees/` | git worktrees of write runs: `<repo key>/<run id>/` on the branch `reno/<run id>`; the repo key is `<name>-<12 hex of the SHA-256 of the repository's canonical git dir>` |
| `~/.reno/` | the definitions, not part of the data directory (see below) |

While the Mac sleeps the scheduler's timer stops, so `renod` listens for the system's wake
notification (IOKit `IORegisterForSystemPower`, see `src/power/`) and tells the scheduler, which
then reads the wall clock and fires what came due. Nothing wakes it periodically.

`RENO_DATA_DIR` and `RENO_SOCKET` are read by `renod`, `reno` and the app alike, for tests and
for running two daemons side by side.

## Terminals

`term.*` (feature `terminals`) holds shells on PTYs for a person: `term.open` starts the person's
login shell (`$SHELL -l`, `TERM=xterm-256color`) in the run's worktree, `term.attach` replays and
follows its output from a sequence number (a 1 MiB window; older is a `Gap`), `term.write` and
`term.resize` are applied in order, `term.close` ends it. The daemon only moves bytes; the app owns
the emulator. At most 16 terminals; a shell that exited is reaped after 30 minutes. Terminals die
with the daemon. Their output is not redacted: it is the person's own shell. `RENO_*` variables are
not passed to it. All of it is person-only: a run's token is refused.

While the thread's latest run is live (queued, running, or interrupted: an interrupted run may be
resumed into the same worktree, by the daemon or a person, so it counts as live), the shell runs under `sandbox-exec` with a profile that
denies writes to what the protected-path guard watches (`.claude`, `.codex`, `.reno`, `.mcp.json`
at any depth, the worktree's `.git` link, and its private git directory under
`<repo>/.git/worktrees/`), from `reno_proto::defs::PROTECTED_DIRS`, so the guard and the sandbox
read one list. The limits outlive the run: `term.list` and `term.status` say `sandboxed`, and a
viewer offers "reopen without limits" (a new `term.open` once the run is over). When
`sandbox-exec` is missing or refuses the profile, `term.open` on a live run fails with
`SandboxUnavailable`; it never opens an unlimited shell. A run with no worktree (`inPlace`) gets a
shell in its own folder, not sandboxed.

Closing is one path in the kernel: a thread that closes (a person, the idle limit, or the end of a
trigger's run) and `workspace.clean` call the `Workspace` port's release hook before they release a
worktree, and the daemon's hook closes that directory's terminals; the result lists them as
`closed_terminals`. A bulk clean (`merged`, `all-clean`) closes them only for a worktree it removes.

Known limits of the sandbox:

- It denies the protected paths and the worktree's private git directory, nothing else in git.
  `git update-ref`, `git branch -f <other>`, `git config`, hooks (`.git/hooks`) and the shared
  `<repo>/.git` are not denied, and the guard does not see them either: it looks at the
  worktree, not at the repository. They are left open on purpose: denying them would break
  ordinary git use in a terminal (`git config`, moving a branch). A hook a person installs runs
  with the person's rights, outside a run's sandbox. `a_sandboxed_git_branch_force_and_git_config_are_allowed_...`
  in `tests/term_sandbox.rs` pins this, so a change is deliberate.
- A shell opened while a run was live keeps its limits after the run ends.
- An interrupted run that can never be resumed still counts as live until it is cancelled.

A daemon started without a PTY spawner (`DaemonConfig::pty`, set only by the binary, and not under
`--no-harnesses`) does not offer the feature. Tests use `FakePty`; `RENO_LIVE_PTY=1 cargo test -p
renod --test term_sandbox live_pty` runs the opt-in tests that start real shells.

## Definitions

Bots, triggers and the policy are TOML files you own, in `~/.reno` (`--defs-dir` or
`RENO_DEFS_DIR` moves it):

```
~/.reno/
  bots/<name>.toml        one bot per file
  triggers/<name>.toml    one trigger per file
  policy.toml             the policy
```

- **First start.** When the directory holds none of `bots/`, `triggers/` and `policy.toml`,
  `renod` writes `bots/example.toml` and `policy.toml` (the safe defaults) and runs `git init`
  there, so every change has a diff and a revert. Without git on `PATH` it logs a warning that
  says so and carries on. Anything that exists is never overwritten.
- **Loading.** The files are read at start and again when `notify` reports a change, after they
  have been quiet for 250 ms. The kernel validates the whole set. A set that validates is stored
  as new versions (only what changed) and published; one that does not is reported through
  `def.list` and `defs.watch`, and the last good set stays in force.
- **Versions.** A version is identified by the SHA-256 of the definition's canonical JSON, so
  comments, whitespace and key order never make one. Versions are immutable. A run submitted
  for a bot (`task.run` with `bot`) is pinned to the version in force at that moment.
- **Checking by hand.** `reno defs check` validates the files without a daemon.

Changes to `.git/` and reads of the files do not trigger a reload.

## Logs

Filtered by `RENO_LOG` (an `EnvFilter` expression such as `renod=debug,reno_kernel=trace`;
`info` by default). When stderr is a terminal the same events are also printed there in a
human-readable form.

## Executors

- `mock` is always registered. It plays a short canned run and finishes. With `RENO_MOCK_HOLD=1`
  it stops after its first text and waits until the run is steered or cancelled; the tests use
  that to keep a run live without a timer.
- `claude-code` and `codex` are registered when their CLIs are found, and so is each agent built
  on ACP, under its own id: `acp.grok`, `acp.devin`, `acp.hermes`, `acp.fx`, `acp.antigravity`,
  and `acp.<registry id>` for each agent added from the ACP registry (never one that would take
  the id of an executor Reno drives itself). `RENO_NO_HARNESSES=1` skips that lookup, so tests
  never touch a real harness.

## Shutdown

`SIGTERM`, `SIGINT` or the `daemon.shutdown` method start the same sequence:

1. stop accepting: the listener is dropped, so the socket file goes and no one new can connect;
2. `Kernel::shutdown`: live runs are cancelled and end `interrupted`; subscribers see their `Done`;
3. open connections get a short grace to read what they are owed, then are cancelled, and the
   server is awaited with a deadline;
4. the socket file is removed if anything kept it;
5. the lock is released, last.

A daemon that is killed (`kill -9`) skips all of this. The next start finds the stale socket and
removes it, and the kernel's recovery interrupts the runs that were live, so each journal ends in
a `Done`.

## Tests

- `tests/protocol.rs` serves the daemon in-process on a temp socket, on a multi-thread runtime,
  and drives it through `reno-client`.
- `tests/process.rs` runs the real binary: the lock, the socket's mode, `SIGTERM`, and a crash.
- `apps/reno/tests/e2e.rs` runs `reno` and `renod` together.

None of them needs a network or a real harness.

## Manual check: a connection from another user is refused

CI cannot do this: it needs a second uid. The socket is already unreachable to other users
(the data directory is 0700), so the check below uses `root`, which can reach the path, to show
that the peer-uid check refuses anyway.

1. Start a daemon as yourself, in a scratch directory:

   ```sh
   export SOCK=/tmp/reno-uid-check/data/reno.sock
   renod --data-dir /tmp/reno-uid-check/data &
   ```

2. As yourself, the request is answered:

   ```sh
   printf '{"jsonrpc":"2.0","id":1,"method":"daemon.status","params":{}}\n' | nc -U "$SOCK"
   ```

   Expected: one JSON line with `"result"` and the daemon's pid.

3. As another user, the same request gets no answer:

   ```sh
   printf '{"jsonrpc":"2.0","id":1,"method":"daemon.status","params":{}}\n' | sudo nc -U "$SOCK"
   ```

   Expected: no output, and the connection closes. The daemon's log (`logs/renod.<date>.log`)
   has a WARN line, `rejecting connection from another user`, with `uid=0`.

4. As an ordinary second user (`sudo -u someoneelse nc -U "$SOCK"`) the connect itself fails with
   `Permission denied`, because the data directory is 0700. That is the first of the two layers.

5. Stop the daemon: `reno --data-dir /tmp/reno-uid-check/data daemon stop`.
