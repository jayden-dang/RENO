//! The ⌘K palette (brief: docs/design/2026-11-19-palette-ui-brief.md): one search over the
//! threads, the focused thread's files and lines, and the commands, in scope tabs. What it
//! shows is `vm::palette`; this view draws it, asks the daemon for files and lines, and tells the
//! shell what was chosen.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    IntoElement, KeyDownEvent, MouseButton, Render, SharedString, StyledText, Subscription, Window,
    div, prelude::*, px,
};
use gpui_tokio::Tokio;
use reno_client::ClientError;
use reno_proto::{FsSearchContentParams, FsSearchFilesParams, RunId};

use super::field::{Field, FieldEvent, Look};
use super::icon::{Icon, icon};
use super::kit::{self, MONO};
use super::sidebar::badge_tag;
use super::theme::{self, Theme};
use crate::nav::Nav;
use crate::store::Store;
use crate::vm::palette::{
    self as vm, Found, Inputs, Item, Kind, Line, Section, Sheet, Tab, Thread,
};

/// What choosing an entry does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    Go(Nav),
    ToggleTheme,
    ToggleSidebar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) label: &'static str,
    pub(crate) hint: &'static str,
    pub(crate) icon: Icon,
    pub(crate) action: PaletteAction,
}

/// Every entry: nav destinations first, then actions.
pub(crate) fn entries() -> Vec<Entry> {
    let go = |nav: Nav| Entry {
        label: nav.label(),
        hint: "Go to",
        icon: nav.icon(),
        action: PaletteAction::Go(nav),
    };
    Nav::ALL
        .into_iter()
        .map(go)
        .chain([
            Entry {
                label: "Toggle theme",
                hint: "Action",
                icon: Icon::Settings,
                action: PaletteAction::ToggleTheme,
            },
            Entry {
                label: "Hide or show sidebar",
                hint: "⌘B",
                icon: Icon::Sidebar,
                action: PaletteAction::ToggleSidebar,
            },
        ])
        .collect()
}

/// The entries matching `query`: case-insensitive, every word must appear in the label. An
/// empty query matches everything. Prefix matches sort first, otherwise order is kept.
pub(crate) fn filter(query: &str, all: &[Entry]) -> Vec<Entry> {
    let query = query.trim().to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut hits: Vec<Entry> = all
        .iter()
        .filter(|e| {
            let label = e.label.to_lowercase();
            words.iter().all(|w| label.contains(w))
        })
        .copied()
        .collect();
    hits.sort_by_key(|e| !e.label.to_lowercase().starts_with(&query));
    hits
}

/// The keycap a command carries.
pub(crate) fn shortcut(action: PaletteAction) -> Option<&'static str> {
    match action {
        PaletteAction::Go(Nav::Settings) => Some("\u{2318},"),
        PaletteAction::ToggleSidebar => Some("\u{2318}B"),
        _ => None,
    }
}

/// What the palette tells the shell.
#[derive(Clone, Debug)]
pub(crate) enum PaletteEvent {
    Run(PaletteAction),
    /// Open this run's thread.
    OpenRun(RunId),
    /// Open a file of the focused thread in its Files tab, at a line when there is one.
    OpenFile {
        path: String,
        line: Option<u32>,
    },
    Close,
}

impl EventEmitter<PaletteEvent> for Palette {}

/// The thread whose project is searched.
#[derive(Clone, Debug)]
pub(crate) struct Scope {
    pub(crate) project: String,
    pub(crate) run: RunId,
}

pub(crate) struct Palette {
    store: Entity<Store>,
    threads: Vec<Thread>,
    scope: Option<Scope>,
    query: String,
    tab: Tab,
    selected: usize,
    files: Found<String>,
    lines: Found<Line>,
    /// The query the last search asked for; an answer for another is dropped.
    asked: String,
    field: Entity<Field>,
    _field: Subscription,
}

impl std::fmt::Debug for Palette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Palette")
            .field("query", &self.query)
            .finish()
    }
}

impl Palette {
    pub(crate) fn new(
        store: Entity<Store>,
        threads: Vec<Thread>,
        scope: Option<Scope>,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = cx.new(|cx| {
            Field::new(
                "palette-input",
                "Search",
                "Search threads, files and commands",
                Look::BARE,
                cx,
            )
        });
        let sub = cx.subscribe(&field, |this: &mut Self, field, event, cx| match event {
            FieldEvent::Changed => {
                let text = field.read(cx).text().to_owned();
                this.set_query(text, cx);
            }
            FieldEvent::Submit => this.activate(cx),
        });
        let none = if scope.is_some() {
            Found::Done(vec![])
        } else {
            Found::NoThread
        };
        Self {
            store,
            threads,
            scope,
            query: String::new(),
            tab: Tab::All,
            selected: 0,
            files: none.clone(),
            lines: match none {
                Found::NoThread => Found::NoThread,
                _ => Found::Done(vec![]),
            },
            asked: String::new(),
            field,
            _field: sub,
        }
    }

    /// What the palette shows now.
    pub(crate) fn sheet(&self) -> Sheet {
        vm::sheet(
            &Inputs {
                threads: &self.threads,
                project: self.scope.as_ref().map(|s| s.project.as_str()),
                files: &self.files,
                lines: &self.lines,
            },
            &self.query,
            self.tab,
        )
    }

    /// The selected row.
    pub(crate) fn selected(&self) -> Option<Item> {
        self.sheet()
            .items()
            .get(self.selected)
            .map(|i| (*i).clone())
    }

    /// Where the file-name search stands.
    #[cfg(test)]
    pub(crate) fn files(&self) -> &Found<String> {
        &self.files
    }

    fn set_query(&mut self, text: String, cx: &mut Context<Self>) {
        self.query = text;
        self.selected = 0;
        self.search(cx);
        cx.notify();
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.selected = 0;
        cx.notify();
    }

    /// Asks the daemon for the focused thread's files and lines matching the query.
    fn search(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.scope.clone() else {
            return;
        };
        let (_, query) = vm::scope(&self.query, self.tab);
        let query = query.trim().to_owned();
        if query.is_empty() {
            self.asked.clear();
            self.files = Found::Done(vec![]);
            self.lines = Found::Done(vec![]);
            return;
        }
        if query == self.asked {
            return;
        }
        self.asked.clone_from(&query);
        let Some(client) = self.store.read(cx).client() else {
            let reason = "Reno's daemon is not connected".to_owned();
            self.files = Found::Failed(reason.clone());
            self.lines = Found::Failed(reason);
            return;
        };
        self.files = Found::Pending;
        self.lines = Found::Pending;
        let files = Tokio::spawn(cx, {
            let client = client.clone();
            let params = FsSearchFilesParams {
                run_id: scope.run.clone(),
                query: query.clone(),
                limit: None,
            };
            async move { client.fs_search_files(params).await }
        });
        let lines = Tokio::spawn(cx, {
            let params = FsSearchContentParams {
                run_id: scope.run,
                query: query.clone(),
                regex: false,
                ignore_case: true,
                limit: None,
            };
            async move { client.fs_search_content(params).await }
        });
        cx.spawn(async move |this, cx| {
            let files = match files.await {
                Ok(Ok(found)) => Found::Done(found.files.into_iter().map(|f| f.path).collect()),
                Ok(Err(error)) => Found::Failed(reason_of(&error)),
                Err(error) => Found::Failed(error.to_string()),
            };
            let lines = match lines.await {
                Ok(Ok(found)) => Found::Done(
                    found
                        .matches
                        .into_iter()
                        .map(|m| Line {
                            path: m.path,
                            line: m.line,
                            text: m.text,
                        })
                        .collect(),
                ),
                Ok(Err(error)) => Found::Failed(reason_of(&error)),
                Err(error) => Found::Failed(error.to_string()),
            };
            this.update(cx, |this, cx| {
                if this.asked == query {
                    this.files = files;
                    this.lines = lines;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn activate_item(&mut self, item: Item, cx: &mut Context<Self>) {
        cx.emit(match item {
            Item::Thread(t) => PaletteEvent::OpenRun(t.run),
            Item::Command(e) => PaletteEvent::Run(e.action),
            Item::File(path) => PaletteEvent::OpenFile { path, line: None },
            Item::Line(l) => PaletteEvent::OpenFile {
                path: l.path,
                line: Some(l.line),
            },
        });
    }

    fn activate(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.selected() {
            self.activate_item(item, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let m = key.modifiers;
        match key.key.as_str() {
            "escape" => cx.emit(PaletteEvent::Close),
            "enter" => self.activate(cx),
            "down" | "up" => {
                let len = self.sheet().items().len();
                self.selected = vm::step(self.selected, len, key.key == "down");
            }
            "tab" => {
                let tab = vm::scope(&self.query, self.tab).0.step(!m.shift);
                self.set_tab(tab, cx);
            }
            digit if m.platform => match digit.parse().ok().and_then(Tab::from_digit) {
                Some(tab) => self.set_tab(tab, cx),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }
}

fn reason_of(error: &ClientError) -> String {
    match error {
        ClientError::Remote(object) => object.message.clone(),
        other => other.to_string(),
    }
}

impl Focusable for Palette {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.field.read(cx).focus_handle(cx)
    }
}

// --- Drawing ------------------------------------------------------------------------------------

/// `text` with the first occurrence of `query` bold and in the accent.
fn marked(theme: &Theme, text: &str, query: &str) -> StyledText {
    let styled = StyledText::new(SharedString::from(text.to_owned()));
    match vm::mark(text, query) {
        Some(range) => styled.with_highlights([(
            range,
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                color: Some(theme.accent),
                ..Default::default()
            },
        )]),
        None => styled,
    }
}

fn faint(theme: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .text_size(px(12.0))
        .text_color(theme.faint)
        .child(text.into())
}

fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn kind_id(kind: Kind) -> &'static str {
    match kind {
        Kind::NeedsYou => "needs",
        Kind::Recent => "recent",
        Kind::Threads => "threads",
        Kind::Files => "files",
        Kind::Text => "text",
        Kind::Commands => "commands",
    }
}

impl Palette {
    fn input_row(&self, theme: &Theme) -> impl IntoElement {
        div()
            .h(px(46.0))
            .px(px(16.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .text_size(px(14.5))
            .child(icon(Icon::Search, 15.0, theme.faint))
            .child(div().flex_1().min_w_0().child(self.field.clone()))
            .children(self.scope.as_ref().map(|scope| {
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(icon(Icon::Folder, 12.0, theme.faint))
                    .child(scope.project.clone())
            }))
    }

    fn tab_row(
        &self,
        theme: &Theme,
        sheet: &Sheet,
        typing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut row = div()
            .h(px(32.0))
            .px(px(10.0))
            .flex()
            .items_end()
            .gap(px(2.0))
            .border_b_1()
            .border_color(theme.hairline);
        for view in &sheet.tabs {
            let tab = view.tab;
            let on = view.active;
            let id = format!("palette-tab-{}", tab.label().to_lowercase());
            let hover = theme.text;
            let label = match view.count {
                Some(n) => format!("{}, {n}", tab.label()),
                None => tab.label().to_owned(),
            };
            row = row.child(
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id.clone())
                    .role(gpui::Role::Button)
                    .aria_label(SharedString::from(label))
                    .aria_toggled(if on {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    })
                    .h_full()
                    .px(px(8.0))
                    .flex()
                    .flex_col()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .text_size(px(12.5))
                            .font_weight(if on {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if on { theme.text } else { theme.muted })
                            .when(!on, |d| d.hover(move |s| s.text_color(hover)))
                            .child(tab.label())
                            .children(view.count.map(|n| {
                                div()
                                    .font_family(MONO)
                                    .text_size(px(11.0))
                                    .text_color(if on { theme.accent } else { theme.faint })
                                    .child(n.to_string())
                            })),
                    )
                    .child(
                        div()
                            .h(px(2.0))
                            .rounded(px(1.0))
                            .when(on, |d| d.bg(theme.accent)),
                    ),
            );
        }
        row.child(div().flex_1()).child(
            div()
                .h_full()
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_size(px(11.0))
                .text_color(theme.faint)
                .child(kit::keycap(theme, "\u{21e5}", None))
                .child(if typing {
                    "scope"
                } else {
                    "> commands  / files  # text"
                }),
        )
    }

    fn section_head(theme: &Theme, section: &Section, first: bool) -> AnyElement {
        div()
            .h(px(26.0))
            .when(!first, |d| d.mt(px(4.0)))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.faint)
            .child(section.kind.title())
            .children(section.finding.clone().map(|f| {
                div()
                    .id("palette-finding")
                    .debug_selector(|| "palette-finding".to_owned())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.warning)
                    .child(f)
            }))
            .child(div().flex_1())
            .children(
                section
                    .count
                    .map(|n| div().font_family(MONO).child(n.to_string())),
            )
            .into_any_element()
    }

    fn row(
        &self,
        theme: &Theme,
        item: &Item,
        index: usize,
        on: bool,
        query: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let line = div()
            .h(px(30.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .text_size(px(13.0))
            .text_color(theme.text);
        let glyph = |el: AnyElement| {
            div()
                .flex_none()
                .w(px(16.0))
                .flex()
                .justify_center()
                .child(el)
        };
        let title = |e: StyledText| div().flex_shrink(1.0).min_w_0().truncate().child(e);
        let mut second: Option<AnyElement> = None;
        let (line, spoken) = match item {
            Item::Thread(t) => {
                if on && let Some(held) = &t.held {
                    second = Some(held_line(
                        theme,
                        held.sentence.clone(),
                        held.command.clone(),
                    ));
                }
                let spoken = match (t.badge, &t.held) {
                    (Some(b), Some(h)) if on => {
                        format!("{}, {}, {}", t.title, b.label(), h.sentence)
                    }
                    (Some(b), _) => format!("{}, {}", t.title, b.label()),
                    _ => t.title.clone(),
                };
                (
                    line.child(glyph(kit::logo(theme, &t.executor, 14.0)))
                        .child(title(marked(theme, &t.title, query)))
                        .child(faint(theme, t.project.clone()))
                        .child(div().flex_1())
                        .children(t.badge.map(|b| badge_tag(theme, b)))
                        .child(
                            faint(theme, t.age.clone())
                                .font_family(MONO)
                                .text_size(px(11.0))
                                .w(px(26.0))
                                .flex()
                                .justify_end(),
                        ),
                    spoken,
                )
            }
            Item::File(path) => {
                let (dir, name) = split_path(path);
                (
                    line.child(glyph(
                        icon(Icon::File, 14.0, theme.muted).into_any_element(),
                    ))
                    .child(title(marked(theme, name, query)))
                    .child(
                        faint(theme, dir.to_owned())
                            .font_family(MONO)
                            .text_size(px(11.5))
                            .min_w_0()
                            .truncate(),
                    ),
                    path.clone(),
                )
            }
            Item::Line(l) => (
                line.child(glyph(
                    icon(Icon::List, 14.0, theme.muted).into_any_element(),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(12.0))
                        .child(marked(theme, l.text.trim(), query)),
                )
                .child(
                    faint(theme, format!("{}:{}", split_path(&l.path).1, l.line))
                        .font_family(MONO)
                        .text_size(px(11.0)),
                ),
                format!("{}, line {}", l.path, l.line),
            ),
            Item::Command(e) => (
                line.child(glyph(icon(e.icon, 14.0, theme.muted).into_any_element()))
                    .child(title(marked(theme, e.label, query)))
                    .child(div().flex_1())
                    .children(shortcut(e.action).map(|k| kit::keycap(theme, k, None))),
                e.label.to_owned(),
            ),
        };
        let id = format!("palette-row-{index}");
        let hover = theme.raised.opacity(0.75);
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id.clone())
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(spoken))
            .flex_none()
            .px(px(10.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .when(on, |r| r.bg(hover))
            .when(!on, |r| r.hover(move |s| s.bg(hover)))
            .on_click({
                let item = item.clone();
                cx.listener(move |this, _, _, cx| this.activate_item(item.clone(), cx))
            })
            .child(line)
            .children(second)
            .into_any_element()
    }
}

/// A selected held action's second line: its sentence, its command, and what ↵ does.
fn held_line(theme: &Theme, sentence: String, command: String) -> AnyElement {
    div()
        .id("palette-held")
        .debug_selector(|| "palette-held".to_owned())
        .pl(px(26.0))
        .pb(px(7.0))
        .mt(px(-3.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(12.0))
        .child(div().flex_none().text_color(theme.text).child(sentence))
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(MONO)
                .text_size(px(11.5))
                .text_color(theme.muted)
                .child(command),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_color(theme.faint)
                .child(kit::keycap(theme, "\u{21b5}", None))
                .child("open"),
        )
        .into_any_element()
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let sheet = self.sheet();
        let len = sheet.items().len();
        if self.selected >= len {
            self.selected = len.saturating_sub(1);
        }
        let (_, query) = vm::scope(&self.query, self.tab);
        let query = query.trim().to_owned();
        let typing = !query.is_empty();

        let mut list = div()
            .id("palette-list")
            .max_h(px(400.0))
            .overflow_y_scroll()
            .px(px(6.0))
            .py(px(4.0))
            .flex()
            .flex_col();
        let mut index = 0;
        for (n, section) in sheet.sections.iter().enumerate() {
            if section.head {
                list = list.child(Self::section_head(&theme, section, n == 0));
            }
            for item in &section.items {
                list =
                    list.child(self.row(&theme, item, index, index == self.selected, &query, cx));
                index += 1;
            }
            if let Some(note) = &section.note {
                let id = format!("palette-note-{}", kind_id(section.kind));
                list = list.child(
                    div()
                        .id(SharedString::from(id.clone()))
                        .debug_selector(move || id.clone())
                        .flex_none()
                        .h(px(30.0))
                        .px(px(38.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(12.5))
                        .text_color(theme.faint)
                        .when(note.busy, |d| {
                            d.child(kit::spinner(
                                SharedString::from(format!(
                                    "palette-busy-{}",
                                    kind_id(section.kind)
                                )),
                                12.0,
                                theme.faint,
                            ))
                        })
                        .child(note.text.clone()),
                );
            }
            if let Some(more) = &section.more {
                list = list.child(
                    div()
                        .flex_none()
                        .h(px(24.0))
                        .px(px(38.0))
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .text_color(theme.faint)
                        .child(more.clone()),
                );
            }
        }
        if let Some(nothing) = &sheet.nothing {
            list =
                list.child(
                    div()
                        .id("palette-nothing")
                        .debug_selector(|| "palette-nothing".to_owned())
                        .px(px(16.0))
                        .py(px(18.0))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_size(px(13.5))
                                .text_color(theme.text)
                                .child(nothing.clone()),
                        )
                        .child(div().text_size(px(12.5)).text_color(theme.faint).child(
                            "Threads match their title or project; files match their path.",
                        )),
                );
        }

        div()
            .id("palette-scrim")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(96.0))
            .bg(gpui::black().opacity(0.35))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Close)))
            .child(
                div()
                    .id("palette")
                    .debug_selector(|| "palette".to_owned())
                    // Keys bubble here from the search field, which keeps the focus: a press
                    // anywhere in the sheet (a tab, a gap) gives it back to the field and keeps
                    // the window from taking it, or typing after a click would go nowhere.
                    .key_context("Palette")
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            let field = this.field.focus_handle(cx);
                            window.focus(&field, cx);
                            window.prevent_default();
                        }),
                    )
                    // A click inside must not reach the scrim.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .w(px(620.0))
                    .rounded(px(12.0))
                    .overflow_hidden()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.hairline)
                    .shadow_lg()
                    .child(self.input_row(&theme))
                    .child(self.tab_row(&theme, &sheet, typing, cx))
                    .child(list),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(hits: &[Entry]) -> Vec<&'static str> {
        hits.iter().map(|e| e.label).collect()
    }

    #[test]
    fn entries_cover_every_destination_and_the_actions() {
        let all = entries();
        assert_eq!(all.len(), 7);
        assert!(all.iter().any(|e| e.action == PaletteAction::ToggleTheme));
        assert!(all.iter().any(|e| e.action == PaletteAction::ToggleSidebar));
        assert_eq!(all[0].action, PaletteAction::Go(Nav::Home));
    }

    #[test]
    fn filter_matches_case_insensitively_by_word() {
        let all = entries();
        assert_eq!(filter("", &all).len(), all.len());
        assert_eq!(labels(&filter("TRIG", &all)), ["Triggers"]);
        assert_eq!(
            labels(&filter("sidebar hide", &all)),
            ["Hide or show sidebar"]
        );
        assert!(filter("zzz", &all).is_empty());
    }

    #[test]
    fn filter_puts_prefix_matches_first() {
        let all = entries();
        let hits = labels(&filter("e", &all));
        let first_non_prefix = hits.iter().position(|l| !l.to_lowercase().starts_with('e'));
        let last_prefix = hits.iter().rposition(|l| l.to_lowercase().starts_with('e'));
        if let (Some(non), Some(last)) = (first_non_prefix, last_prefix) {
            assert!(last < non, "{hits:?}");
        }
        assert_eq!(hits[0], "Executors");
    }
}
