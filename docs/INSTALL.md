# Running Reno from source

There is no release yet. These steps build Reno from a checkout and run the daemon under launchd
on macOS.

## Build

```sh
cargo build --release -p renod -p reno
```

The binaries are `target/release/renod` (the daemon) and `target/release/reno` (the CLI).

## Install the binaries

Put both binaries in one directory on your `PATH`. The CLI looks for `renod` next to itself
first. This guide uses `~/.local/bin`:

```sh
mkdir -p ~/.local/bin
for b in reno renod; do
  cp target/release/$b ~/.local/bin/.$b.new
  mv -f ~/.local/bin/.$b.new ~/.local/bin/$b
done
```

**Copy to a temporary file, then `mv` it over the old one. Never `cp` straight onto a binary
that may be running.** `cp` onto an existing file rewrites that file in place, keeping its
inode, and the kernel still holds the code signature it checked for that file. The changed
file no longer matches it, so every later launch is killed with `SIGKILL` (exit status 137)
before it prints anything. launchd keeps restarting it, so the daemon looks hung. `mv`
replaces the directory entry with a new inode, and the new binary starts cleanly. If you
already hit this, install again with `mv`.

## Run the daemon under launchd

```sh
reno daemon install
```

This writes `~/Library/LaunchAgents/dev.reno.renod.plist` and loads it. launchd then starts the
daemon at login and restarts it after a crash. The plist names `renod` by its resolved path,
so install the binaries before you run this. Use `reno daemon install --dry-run` to see what
it would write.

Check that it is up and that your harness CLIs are found and signed in:

```sh
reno daemon status
reno executor list
```

## Update

Build, install the binaries with the copy-then-`mv` step above, and restart the daemon:

```sh
launchctl kickstart -k gui/$(id -u)/dev.reno.renod
```

`reno daemon restart` does the same. Runs in progress are interrupted, and those that allow it
resume on their own once the daemon is back.

## Where things live

| What | Where |
|---|---|
| Definitions (bots, triggers, policy) | `~/.reno`, a git repository; override with `RENO_DEFS_DIR` |
| Store, journals, logs, socket | `~/Library/Application Support/Reno`; override with `--data-dir` or `RENO_DATA_DIR` |
| Daemon log | `reno daemon logs` (`--follow` to keep reading) |

## Remove

```sh
reno daemon uninstall
rm ~/.local/bin/reno ~/.local/bin/renod
```

`uninstall` stops the daemon and deletes the plist. Your definitions and data are left in place.
