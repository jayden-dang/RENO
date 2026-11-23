# Third-party notices

Reno's own source code is licensed under the terms in `LICENSE`. Bundled third-party components
keep their own licenses and notices.

| Component | Version | License | Where | Source |
|---|---|---|---|---|
| Geist, Geist Mono | 1.x | SIL Open Font License 1.1 | `docs/media/src/fonts/` | https://github.com/vercel/geist-font |
| KaTeX fonts (21 `KaTeX_*.ttf` files, embedded in the app through the `ratex-katex-fonts` crate, which draws typeset math) | as in `ratex-katex-fonts` 0.1.16 | SIL Open Font License 1.1 | compiled into `reno-app` | https://github.com/KaTeX/KaTeX |

The KaTeX fonts carry "Copyright (c) 2009-2010 Design Science, Inc." and "Copyright (c) 2014-2018 Khan Academy".
The full font licenses are in `docs/media/src/fonts/Geist-OFL.txt` and
`apps/reno-app/assets/fonts/KaTeX-OFL.txt`. The KaTeX project's own MIT license covers its software,
not the font files.
