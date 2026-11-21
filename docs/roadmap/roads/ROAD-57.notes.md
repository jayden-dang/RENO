# ROAD-57 thread-links-open-the-app — notes

From ROAD-38 (2026-11-21). The row menu already copies `reno://t/<thread id>` and the app has
one `open_link(url)` entry that focuses that thread (`apps/reno-app/src/vm/thread_link.rs`), but
nothing outside the app can open such a link yet: the URL scheme has to be declared in the
bundle's Info.plist (`cargo xtask bundle`) and the app has to receive the open-URL event.
The CLI side (`reno run url` or similar) is part of the same item (ARCH-2).
