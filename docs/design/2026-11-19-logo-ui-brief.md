# Reno logo — UI brief (locked 2026-11-19)

Screenshots: `2026-11-19-logo-icon.png` (the 1024 px icon from `Reno.icns`) and
`2026-11-19-logo-sidebar.png` (the in-app mark in the sidebar, light theme, fake daemon).

- **Decision:** variant B, "Sibling". The owner asked to redesign Reno's logo "theo style của dự
  án klynt" and picked B over A (Stencil: solid stem, triangle counter), C (Arrow: the stem's
  head points into the bowl) and D (Block: no counter, the gutters draw the R). For the app
  icon the owner chose paper on ink over ink on paper and over Reno's purple on ink. They also
  asked for the mark inside the app.
- **Grounding:** the Klynt mark (`klynt/apps/web/src/shared/brand/KlyntMark.tsx`): three
  solid polygons, edges at 0/45/90 degrees only, one even gutter (15.7 on a 233 box, about
  6.7%), a heavy chevron, a single `currentColor` fill, ink `#1d1c1a` and paper `#ecebe6`. No
  Reno token is added or changed. `theme.rs` keeps its accent `#8b7cf6`; the icon simply no
  longer uses it.

## Construction

On a 240 grid with a 16 gutter (D = 16·√2 ≈ 22.6, the horizontal step of a 45° gutter):

| Piece | Points |
|---|---|
| Stem, upper triangle | (0,0) (56,56) (0,112) |
| Bowl (">" chevron, its notch nests the triangle) | (D,0) (D+104,0) (D+160,56) (D+104,112) (D,112) (D+56,56) |
| Stem, lower triangle | (0,128) (56,184) (0,240) |
| Leg (the K's lower arm, kept) | (D,128) (D+104,128) (D+216,240) (D+112,240) |

It is the Klynt K with the upper arm turned into a bowl: the same split stem, the same leg.
Gutters are even at 16 between all four pieces.

## Surface 1 — App icon (`assets/app-icon`)

- **Layout:** macOS grid, an 824 px rounded square (rx 185) on the 1024 canvas. The plate is
  flat ink `#1d1c1a` with an 8% white hairline at the edge. The mark is paper `#ecebe6`,
  430 px tall, and centred.
- **Small sizes:** the 16 and 32 px slots (16, 16@2x, 32) come from `reno-small.svg`, the
  same drawing with the mark 540 px tall. At 430 px the gutters close up below 64 px.
- **Components:** new. `reno.svg` and `reno-small.svg` are the sources, `Reno.icns` is
  generated from them, and the recipe is in the `README.txt` there.
- **Type & color:** no gradient, no stroke on the mark, no text.

## Surface 2 — In-app mark (sidebar titlebar band)

- **Layout:** the first item in the sidebar's 40 px titlebar band, 14 px, starting at
  `sidebar::MARK_INSET` (84 px, clear of the traffic lights). A flexible spacer follows, then
  the hide-sidebar and new-task buttons. The band shown while the sidebar is hidden uses the
  same inset for its button and has no mark.
- **Components:** rung 1, `ui::icon::icon` with the new `Icon::Mark` (`assets/icons/mark.svg`,
  16 grid, `fill="currentColor"`). It is painted in `theme.text`, the colour of the words
  around it.
- **States:** none. It is not a control.
- **A11y:** decorative; the window title carries the name.

## Signature

The K's split stem kept in the R, so the two marks read as one family side by side.

## Amendments

- None from the owner. During the build the 16 and 32 px icon slots were given a larger mark
  (`reno-small.svg`) after a 4× pixel zoom showed B's triangles blurring at 430 px. The owner
  has yet to review this change.
