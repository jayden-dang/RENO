//! Public-API check: feed a colored line and cursor movement, read the grid back.

use reno_term::emulator::{CellColor, Emulator};

#[test]
fn a_colored_line_and_cursor_movement_land_in_the_grid() {
    let mut term = Emulator::new(20, 4);
    // Green "ok", reset, " done"; CR LF, "next"; then up one row and right two columns.
    term.feed(b"\x1b[32mok\x1b[0m done\r\nnext\x1b[1A\x1b[2C");

    assert_eq!(term.row_text(0), "ok done");
    assert_eq!(term.row_text(1), "next");

    let first = term.line(0);
    assert_eq!(first[0].ch, 'o');
    assert_eq!(first[0].fg, CellColor::Indexed(2));
    assert_eq!(first[1].fg, CellColor::Indexed(2));
    assert_eq!(first[2].fg, CellColor::Foreground, "reset ends the color");

    let cursor = term.cursor().map(|c| (c.row, c.col));
    assert_eq!(
        cursor,
        Some((0, 6)),
        "next ended at col 4; up one, right two"
    );
}

#[test]
fn bracketed_paste_and_resize_are_visible_through_the_api() {
    let mut term = Emulator::new(10, 2);
    assert!(!term.bracketed_paste_mode());
    term.feed(b"\x1b[?2004h");
    assert!(term.bracketed_paste_mode());
    term.resize(14, 3);
    assert_eq!((term.cols(), term.rows()), (14, 3));
}
