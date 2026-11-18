//! A small text field: the search of the registry, the name and command of your own agent, the
//! environment of an agent. One entity per field, on the composer's pure [`TextModel`], so
//! typing, the caret, a selection, paste and undo behave the same.
//!
//! What it leaves out, deliberately: no IME composition (typed characters arrive as key
//! events), and a click focuses the field without placing the caret. The text lives only here:
//! nothing in the field is logged or put in a `Debug` (an environment holds keys).

use gpui::{
    ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement,
    KeyDownEvent, Modifiers, Render, SharedString, Window, div, prelude::*, px,
};

use super::kit::MONO;
use super::text_model::{EnterOutcome, TextModel};
use super::theme::{self, Theme};

/// What a field tells its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldEvent {
    /// The text changed.
    Changed,
    /// Enter in a one-line field.
    Submit,
}

/// How a field looks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Look {
    /// A rounded pill (the search) rather than a box.
    pub(crate) pill: bool,
    pub(crate) mono: bool,
    pub(crate) multiline: bool,
    /// Rows tall when multi-line.
    pub(crate) rows: usize,
    pub(crate) width: Option<f32>,
}

impl Look {
    pub(crate) const SEARCH: Look = Look {
        pill: true,
        mono: false,
        multiline: false,
        rows: 1,
        width: Some(240.0),
    };
    pub(crate) const LINE: Look = Look {
        pill: false,
        mono: false,
        multiline: false,
        rows: 1,
        width: None,
    };
    pub(crate) const COMMAND: Look = Look {
        pill: false,
        mono: true,
        multiline: false,
        rows: 1,
        width: None,
    };
    pub(crate) const ENV: Look = Look {
        pill: false,
        mono: true,
        multiline: true,
        rows: 4,
        width: None,
    };
}

pub(crate) struct Field {
    model: TextModel,
    focus: FocusHandle,
    /// The element id and test selector: "registry-search".
    id: SharedString,
    /// The accessible label.
    label: SharedString,
    placeholder: SharedString,
    look: Look,
}

impl EventEmitter<FieldEvent> for Field {}

impl Focusable for Field {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// What a key did to the text, for the owner to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeyOutcome {
    Edited,
    Moved,
    Submit,
    /// Text for the clipboard (copy, or cut when `edited`).
    Copy {
        text: String,
        edited: bool,
    },
    /// Paste what the clipboard holds.
    Paste,
    Ignored,
}

/// The editing keys, as a pure function of the model so each is a unit test.
pub(crate) fn apply_key(
    model: &mut TextModel,
    key: &str,
    key_char: Option<&str>,
    m: Modifiers,
    multiline: bool,
) -> KeyOutcome {
    let extend = m.shift;
    if m.platform {
        return match key {
            "a" => {
                model.select_all();
                KeyOutcome::Moved
            }
            "v" => KeyOutcome::Paste,
            "c" => model
                .selected_text()
                .map_or(KeyOutcome::Ignored, |t| KeyOutcome::Copy {
                    text: t.to_owned(),
                    edited: false,
                }),
            "x" => model
                .cut()
                .map_or(KeyOutcome::Ignored, |text| KeyOutcome::Copy {
                    text,
                    edited: true,
                }),
            "z" if m.shift => edited(model.redo()),
            "z" => edited(model.undo()),
            "backspace" => edited(model.delete_line_back()),
            "left" => moved(|| model.move_home(extend)),
            "right" => moved(|| model.move_end(extend)),
            "up" => moved(|| model.move_doc_start(extend)),
            "down" => moved(|| model.move_doc_end(extend)),
            _ => KeyOutcome::Ignored,
        };
    }
    if m.alt {
        match key {
            "backspace" => return edited(model.delete_word_back()),
            "delete" => return edited(model.delete_word_forward()),
            "left" => return moved(|| model.move_word_left(extend)),
            "right" => return moved(|| model.move_word_right(extend)),
            _ => {}
        }
    }
    match key {
        "backspace" => edited(model.backspace()),
        "delete" => edited(model.delete()),
        "left" => moved(|| model.move_left(extend)),
        "right" => moved(|| model.move_right(extend)),
        "up" if multiline => moved(|| model.move_up(extend)),
        "down" if multiline => moved(|| model.move_down(extend)),
        "home" => moved(|| model.move_home(extend)),
        "end" => moved(|| model.move_end(extend)),
        "enter" if multiline => {
            model.insert("\n");
            KeyOutcome::Edited
        }
        "enter" => match model.enter(false) {
            // The owner reads the text, so it stays: put it back.
            EnterOutcome::Submitted(text) => {
                model.set_text(&text);
                KeyOutcome::Submit
            }
            _ => KeyOutcome::Ignored,
        },
        _ => match key_char {
            Some(text) if !m.control && !text.chars().any(char::is_control) => {
                model.insert(text);
                KeyOutcome::Edited
            }
            _ => KeyOutcome::Ignored,
        },
    }
}

fn edited(did: bool) -> KeyOutcome {
    if did {
        KeyOutcome::Edited
    } else {
        KeyOutcome::Ignored
    }
}

fn moved(go: impl FnOnce()) -> KeyOutcome {
    go();
    KeyOutcome::Moved
}

impl Field {
    pub(crate) fn new(
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        look: Look,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            model: TextModel::new(),
            focus: cx.focus_handle(),
            id: id.into(),
            label: label.into(),
            placeholder: placeholder.into(),
            look,
        }
    }

    pub(crate) fn text(&self) -> &str {
        self.model.text()
    }

    /// Replaces the text; the caret goes to its end. Does not tell the owner.
    pub(crate) fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.model.set_text(text);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let outcome = apply_key(
            &mut self.model,
            key.key.as_str(),
            key.key_char.as_deref(),
            key.modifiers,
            self.look.multiline,
        );
        match outcome {
            KeyOutcome::Edited => cx.emit(FieldEvent::Changed),
            KeyOutcome::Submit => cx.emit(FieldEvent::Submit),
            KeyOutcome::Copy { text, edited } => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                if edited {
                    cx.emit(FieldEvent::Changed);
                }
            }
            KeyOutcome::Paste => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let text = if self.look.multiline {
                        text
                    } else {
                        text.replace(['\n', '\r'], " ")
                    };
                    self.model.insert(&text);
                    cx.emit(FieldEvent::Changed);
                }
            }
            KeyOutcome::Moved => {}
            KeyOutcome::Ignored => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

/// One line of text cut into runs by the selection, with the caret's offset in it.
#[derive(Debug, PartialEq, Eq)]
struct Runs {
    before: String,
    selected: String,
    after: String,
}

/// The parts of `line` (starting at byte `start` of the text) around the selection.
fn runs_of(line: &str, start: usize, selection: &std::ops::Range<usize>) -> Runs {
    let end = start + line.len();
    let from = selection.start.clamp(start, end) - start;
    let to = selection.end.clamp(start, end) - start;
    Runs {
        before: line[..from].to_owned(),
        selected: line[from..to].to_owned(),
        after: line[to..].to_owned(),
    }
}

/// What a line is painted from: a run of text (selected or not), or the caret between runs.
#[derive(Debug, PartialEq, Eq)]
enum Piece {
    Text(String, bool),
    Caret,
}

/// The runs of a line with the caret cut into them at byte `caret` of the line.
fn pieces(runs: &Runs, caret: Option<usize>) -> Vec<Piece> {
    let mut out = vec![];
    let mut offset = 0;
    let mut placed = caret.is_none();
    for (text, selected) in [
        (&runs.before, false),
        (&runs.selected, true),
        (&runs.after, false),
    ] {
        let end = offset + text.len();
        match caret {
            Some(at) if !placed && at >= offset && at <= end => {
                let (x, y) = text.split_at(at - offset);
                out.push(Piece::Text(x.to_owned(), selected));
                out.push(Piece::Caret);
                out.push(Piece::Text(y.to_owned(), selected));
                placed = true;
            }
            _ => out.push(Piece::Text(text.clone(), selected)),
        }
        offset = end;
    }
    out
}

impl Render for Field {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme: Theme = theme::current(cx);
        let focused = self.focus.is_focused(window);
        let look = self.look;
        let line_h = if look.multiline { 18.0 } else { 20.0 };
        let text = self.model.text().to_owned();
        let selection = self.model.selection();
        let caret = self.model.caret();
        let empty = text.is_empty();

        let mut start = 0;
        let mut lines = vec![];
        for (n, line) in text.split('\n').enumerate() {
            let runs = runs_of(line, start, &selection);
            let end = start + line.len();
            let caret_here = focused && caret >= start && caret <= end;
            let selected_bg: Hsla = theme.accent.opacity(0.3);
            let mut row = div()
                .id(SharedString::from(format!("{}-line-{n}", self.id)))
                .flex()
                .items_center()
                .h(px(line_h))
                .min_w_0();
            let at = (caret_here).then(|| caret - start);
            for piece in pieces(&runs, at) {
                row = match piece {
                    Piece::Caret => row.child(
                        div()
                            .flex_none()
                            .w(px(1.5))
                            .h(px(line_h - 4.0))
                            .bg(theme.text),
                    ),
                    Piece::Text(text, _) if text.is_empty() => row,
                    Piece::Text(text, selected) => row.child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .when(selected, |d| d.bg(selected_bg))
                            .child(text),
                    ),
                };
            }
            lines.push(row);
            start = end + 1;
        }

        let selector = self.id.to_string();
        let body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .when(!look.multiline && focused, |d| d.items_end())
            .when(empty && !focused, |d| {
                d.child(
                    div()
                        .h(px(line_h))
                        .whitespace_nowrap()
                        .text_color(theme.faint)
                        .child(self.placeholder.clone()),
                )
            })
            .when(!(empty && !focused), |d| d.children(lines));
        div()
            .id(self.id.clone())
            .debug_selector(move || selector.clone())
            .role(gpui::Role::TextInput)
            .aria_label(self.label.clone())
            .aria_value(SharedString::from(
                if self.look.mono && self.look.multiline {
                    // An environment holds keys: a screen reader is not read them.
                    String::new()
                } else {
                    text
                },
            ))
            .aria_placeholder(self.placeholder.clone())
            .track_focus(&self.focus)
            .tab_stop(true)
            .key_context("RegistryField")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.focus, cx);
            }))
            .cursor_text()
            .flex()
            .flex_col()
            .when_some(look.width, |d, w| d.w(px(w)))
            .when(look.width.is_none(), |d| d.w_full())
            .min_w_0()
            .px(px(if look.pill { 14.0 } else { 10.0 }))
            .py(px(if look.multiline { 8.0 } else { 5.0 }))
            .when(look.multiline, |d| {
                d.min_h(px(line_h * look.rows as f32 + 16.0))
            })
            .rounded(px(if look.pill { 999.0 } else { 8.0 }))
            .bg(theme.raised)
            .border_1()
            .border_color(if focused {
                theme.accent
            } else {
                theme.hairline
            })
            .text_size(px(if look.mono { 12.0 } else { 13.0 }))
            .when(look.mono, |d| d.font_family(MONO))
            .font_weight(FontWeight::NORMAL)
            .text_color(theme.text)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_str(model: &mut TextModel, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            apply_key(model, &s, Some(&s), Modifiers::none(), false);
        }
    }

    fn cmd() -> Modifiers {
        Modifiers {
            platform: true,
            ..Modifiers::none()
        }
    }

    #[test]
    fn typing_and_backspace_edit_the_text() {
        let mut m = TextModel::new();
        type_str(&mut m, "kilo");
        assert_eq!(m.text(), "kilo");
        assert_eq!(
            apply_key(&mut m, "backspace", None, Modifiers::none(), false),
            KeyOutcome::Edited
        );
        assert_eq!(m.text(), "kil");
        // Nothing to delete at the start: not an edit.
        apply_key(&mut m, "home", None, Modifiers::none(), false);
        assert_eq!(
            apply_key(&mut m, "backspace", None, Modifiers::none(), false),
            KeyOutcome::Ignored
        );
    }

    #[test]
    fn enter_submits_a_one_line_field_and_adds_a_line_in_a_multiline_one() {
        let mut one = TextModel::new();
        type_str(&mut one, "name");
        assert_eq!(
            apply_key(&mut one, "enter", None, Modifiers::none(), false),
            KeyOutcome::Submit
        );
        assert_eq!(one.text(), "name", "the text stays for the owner to read");
        let mut many = TextModel::new();
        type_str(&mut many, "A=1");
        apply_key(&mut many, "enter", None, Modifiers::none(), true);
        type_str(&mut many, "B=2");
        assert_eq!(many.text(), "A=1\nB=2");
    }

    #[test]
    fn select_all_and_typing_replace_and_the_clipboard_keys_say_what_to_copy() {
        let mut m = TextModel::new();
        type_str(&mut m, "old");
        assert_eq!(
            apply_key(&mut m, "a", None, cmd(), false),
            KeyOutcome::Moved
        );
        assert_eq!(
            apply_key(&mut m, "c", None, cmd(), false),
            KeyOutcome::Copy {
                text: "old".into(),
                edited: false
            }
        );
        type_str(&mut m, "new");
        assert_eq!(m.text(), "new");
        assert_eq!(
            apply_key(&mut m, "v", None, cmd(), false),
            KeyOutcome::Paste
        );
        apply_key(&mut m, "a", None, cmd(), false);
        assert_eq!(
            apply_key(&mut m, "x", None, cmd(), false),
            KeyOutcome::Copy {
                text: "new".into(),
                edited: true
            }
        );
        assert!(m.is_empty());
        // Undo brings the cut text back.
        assert_eq!(
            apply_key(&mut m, "z", None, cmd(), false),
            KeyOutcome::Edited
        );
        assert_eq!(m.text(), "new");
    }

    #[test]
    fn control_characters_and_shortcuts_are_never_typed() {
        let mut m = TextModel::new();
        assert_eq!(
            apply_key(&mut m, "tab", Some("\t"), Modifiers::none(), false),
            KeyOutcome::Ignored
        );
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::none()
        };
        assert_eq!(
            apply_key(&mut m, "a", Some("a"), ctrl, false),
            KeyOutcome::Ignored
        );
        assert!(m.is_empty());
    }

    #[test]
    fn the_caret_is_cut_into_the_run_it_sits_in() {
        let runs = runs_of("KEY=VALUE", 0, &(0..0));
        let texts: Vec<_> = pieces(&runs, Some(3))
            .into_iter()
            .filter(|p| !matches!(p, Piece::Text(t, _) if t.is_empty()))
            .collect();
        assert_eq!(
            texts,
            [
                Piece::Text("KEY".into(), false),
                Piece::Caret,
                Piece::Text("=VALUE".into(), false)
            ]
        );
        // No focus, no caret.
        assert!(!pieces(&runs, None).contains(&Piece::Caret));
        // A caret at the end of a selection sits after its last character.
        let runs = runs_of("abc", 0, &(1..3));
        let all = pieces(&runs, Some(3));
        let caret_at = all.iter().position(|p| *p == Piece::Caret).unwrap();
        assert_eq!(all[caret_at - 1], Piece::Text("bc".into(), true));
    }

    #[test]
    fn a_line_is_cut_around_the_selection() {
        let runs = runs_of("KEY=VALUE", 10, &(12..15));
        assert_eq!(
            (
                runs.before.as_str(),
                runs.selected.as_str(),
                runs.after.as_str()
            ),
            ("KE", "Y=V", "ALUE")
        );
        // A selection on other lines leaves this one whole.
        let runs = runs_of("KEY", 10, &(0..5));
        assert_eq!(runs.selected, "");
        assert_eq!(runs.before, "");
    }
}
