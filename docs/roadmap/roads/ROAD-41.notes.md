# ROAD-41 app-and-daemon-self-update — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Channel baked in at compile time from a one-word file (`crates/zed/RELEASE_CHANNEL`, read by `crates/release_channel/src/lib.rs`); one version source; `script/determine-release-channel` checks the tag matches the crate version (GPL).
- macOS signing and notarization: `script/bundle-mac` (temporary keychain, `codesign --options runtime`, `notarytool submit --wait` with an API key, `stapler staple`).
- Install: `hdiutil attach -nobrowse`, `rsync -av --delete` over the running bundle, unmount in a drop guard, clean installer dirs older than 24 h: `crates/auto_update/src/auto_update.rs` ~1197-1257 (GPL).
- A status enum (Idle, Checking, Downloading, Installing, Updated, Errored) and a tested "is newer" predicate (~123, ~1622).
