//! The Files tab of the side panel: a read-only tree and viewer over the run's worktree.
//!
//! `vm::files` decides everything; this file sends what it asks the daemon for, applies the
//! answers, holds the `fs.watch` subscription (only while the tab is the visible one) and draws
//! the tree, the viewer and the search.

use std::future::Future;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AnyElement, Context, Entity, FontWeight, Hsla, ImageFormat, ObjectFit, SharedString,
    StatefulInteractiveElement as _, StyledText, Subscription, Task, UniformListScrollHandle,
    Window, div, img, prelude::*, px, uniform_list,
};
use gpui::{ListHorizontalSizingBehavior, ScrollStrategy};
use gpui_tokio::Tokio;
use reno_client::{Client, ClientError};
use reno_proto::{
    ErrorCode, FsChange, FsListParams, FsReadParams, FsSearchContentParams, FsSearchFilesParams,
    RunId,
};
use reno_syntax::{HighlightRequest, HighlightedDocument};

use super::RunScreen;
use super::output::copy;
use crate::panel::surface::Surface;
use crate::panel::view::Prefs;
use crate::ui::composer::{Composer, ReasonEvent};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::markdown_view::{self, Streaming};
use crate::ui::theme::Theme;
use crate::vm::files::{
    Deco, DocState, Fail, FilesModel, Highlight, Live, Note, Request, Results, Row, SearchMode,
    View,
};

/// Below this panel width the tree and the viewer stack, one page at a time.
pub(super) const NARROW_BELOW: f32 = 560.0;
/// The tree's share of a wide panel.
const TREE_SHARE: f32 = 0.38;
/// How long typing waits before the search is sent.
pub(super) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);
/// The height of a tree row, and of a line of code.
const ROW: f32 = 26.0;
const LINE: f32 = 19.0;
/// The height of a search result.
const RESULT: f32 = 44.0;

/// What the watch task tells the screen.
enum WatchMsg {
    Started,
    Item(FsChange),
    Failed(String),
    Ended,
}

/// The two tasks that hold an `fs.watch` open. Dropping them cancels the pump, which drops the
/// stream, which ends the daemon's subscription.
struct Watch {
    _pump: Task<Result<(), gpui_tokio::JoinError>>,
    _reader: Task<()>,
}

/// One run's Files tab, kept while the run screen is open (the tab shows and hides, this stays).
pub(super) struct FilesTab {
    pub(super) model: FilesModel,
    search: Entity<Composer>,
    _search_events: [Subscription; 2],
    search_timer: Option<Task<()>>,
    watch: Option<Watch>,
    /// The tab was hidden since it last read: the next show reads everything shown again.
    stale: bool,
    tree_scroll: UniformListScrollHandle,
    results_scroll: UniformListScrollHandle,
    code_scroll: UniformListScrollHandle,
    /// The open markdown's parse, and what it was made from.
    md: Streaming,
    md_for: Option<(String, Option<String>)>,
    /// The open image as GPUI wants it, and the bytes it was made from.
    image: Option<(Arc<Vec<u8>>, Arc<gpui::Image>)>,
    /// The document the code list shows, so a new one starts at the top.
    shown: Option<String>,
    /// The text a highlight task is running for.
    highlighting: Option<(String, Option<String>)>,
}

fn fail_of(error: &ClientError) -> Fail {
    match error {
        ClientError::Remote(object) => match object.code {
            ErrorCode::WorkspaceUnavailable => Fail::Unavailable(object.message.clone()),
            ErrorCode::NotFound => Fail::NotFound(object.message.clone()),
            ErrorCode::InvalidParams
                if object
                    .data
                    .as_ref()
                    .and_then(|d| d.get("kind"))
                    .and_then(|k| k.as_str())
                    == Some("too_large") =>
            {
                Fail::TooLarge
            }
            _ => Fail::Other(object.message.clone()),
        },
        other => Fail::Other(other.to_string()),
    }
}

impl RunScreen {
    // --- when it runs -------------------------------------------------------------------------

    /// Whether the Files tab is what a person can see now: it is the panel's tab in front, and
    /// the panel is drawn. A watch lives exactly as long as this holds (ARCH-10).
    fn files_visible(&self, cx: &gpui::App) -> bool {
        *self.panel.active() == Surface::Files
            && self.panel_shown
            && Prefs::get(cx).open != Some(false)
    }

    /// Starts, stops or refreshes the tab to match whether it shows. Called wherever the panel's
    /// tabs change and when the store does.
    pub(super) fn sync_files(&mut self, cx: &mut Context<Self>) {
        if !self.files_visible(cx) {
            if let Some(tab) = self.files.as_mut()
                && (tab.watch.take().is_some() || tab.model.live != Live::Off)
            {
                tab.model.set_live(Live::Off);
                tab.stale = true;
            }
            return;
        }
        let (supported, connected) = {
            let store = self.store.read(cx);
            (store.supports_files(), store.client().is_some())
        };
        if !supported || !connected {
            return;
        }
        if self.files.is_none() {
            let tab = self.new_files_tab(cx);
            self.files = Some(tab);
            let reqs = self.files.as_mut().map(|t| t.model.start());
            self.run_files(reqs.unwrap_or_default(), cx);
        } else if let Some(tab) = self.files.as_mut()
            && std::mem::take(&mut tab.stale)
        {
            let reqs = tab.model.rescan();
            self.run_files(reqs, cx);
        }
        if self.files.as_ref().is_some_and(|t| t.watch.is_none()) {
            self.start_watch(cx);
        }
    }

    fn new_files_tab(&mut self, cx: &mut Context<Self>) -> FilesTab {
        let store = self.store.clone();
        let search = cx.new(|cx| {
            Composer::field(
                store,
                "Find a file",
                "> searches inside files \u{b7} Esc clears",
                cx,
            )
        });
        let observed = cx.observe(&search, |this, field, cx| {
            let text = field.read(cx).typed_reason().unwrap_or_default();
            this.files_search_text(&text, cx);
        });
        let events = cx.subscribe(&search, |this, _, event: &ReasonEvent, cx| {
            if let ReasonEvent::Submitted(_) = event {
                // Enter does not wait out the debounce.
                this.commit_files_search(cx);
            }
        });
        FilesTab {
            model: FilesModel::new(),
            search,
            _search_events: [observed, events],
            search_timer: None,
            watch: None,
            stale: false,
            tree_scroll: UniformListScrollHandle::new(),
            results_scroll: UniformListScrollHandle::new(),
            code_scroll: UniformListScrollHandle::new(),
            md: Streaming::default(),
            md_for: None,
            image: None,
            shown: None,
            highlighting: None,
        }
    }

    // --- talking to the daemon ----------------------------------------------------------------

    pub(super) fn files_call<T, Fut>(
        &mut self,
        cx: &mut Context<Self>,
        call: impl FnOnce(Client, RunId) -> Fut,
        done: impl FnOnce(&mut Self, Result<T, Fail>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        let Some(client) = self.store.read(cx).client() else {
            done(
                self,
                Err(Fail::Other("the daemon is not connected".to_owned())),
                cx,
            );
            return;
        };
        let task = Tokio::spawn(cx, call(client, self.run.clone()));
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(fail_of(&error)),
                Err(error) => Err(Fail::Other(error.to_string())),
            };
            this.update(cx, |screen, cx| done(screen, result, cx)).ok();
        })
        .detach();
    }

    /// Runs `apply` on the model and then everything that follows from a change to it.
    fn with_files(&mut self, cx: &mut Context<Self>, apply: impl FnOnce(&mut FilesModel)) {
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        apply(&mut tab.model);
        self.files_changed(cx);
    }

    /// Sends what the model asked for.
    fn run_files(&mut self, requests: Vec<Request>, cx: &mut Context<Self>) {
        for request in requests {
            match request {
                Request::List {
                    dir,
                    cursor,
                    ticket,
                } => {
                    let (ask_dir, ask_cursor) = (dir.clone(), cursor.clone());
                    self.files_call(
                        cx,
                        move |client, run| async move {
                            client
                                .fs_list(FsListParams {
                                    run_id: run,
                                    dir: ask_dir,
                                    cursor: ask_cursor,
                                    limit: None,
                                })
                                .await
                        },
                        move |this, result, cx| {
                            this.with_files(cx, |m| {
                                m.apply_list(&dir, ticket, cursor.as_deref(), result);
                            });
                        },
                    );
                }
                Request::Read { path, ticket } => self.files_call(
                    cx,
                    move |client, run| async move {
                        client.fs_read(FsReadParams { run_id: run, path }).await
                    },
                    move |this, result, cx| this.with_files(cx, |m| m.apply_read(ticket, result)),
                ),
                Request::Image { path, ticket } => self.files_call(
                    cx,
                    move |client, run| async move { client.fs_read_image_all(run, path).await },
                    move |this, result, cx| this.with_files(cx, |m| m.apply_image(ticket, result)),
                ),
                Request::Status { ticket } => self.files_call(
                    cx,
                    move |client, run| async move { client.git_status(run).await },
                    move |this, result, cx| {
                        this.with_files(cx, |m| {
                            m.apply_status(ticket, result);
                        });
                    },
                ),
                Request::SearchFiles { query, ticket } => self.files_call(
                    cx,
                    move |client, run| async move {
                        client
                            .fs_search_files(FsSearchFilesParams {
                                run_id: run,
                                query,
                                limit: None,
                            })
                            .await
                    },
                    move |this, result, cx| {
                        this.with_files(cx, |m| m.apply_search_files(ticket, result));
                    },
                ),
                Request::SearchContent { query, ticket } => self.files_call(
                    cx,
                    move |client, run| async move {
                        client
                            .fs_search_content(FsSearchContentParams {
                                run_id: run,
                                query,
                                regex: false,
                                ignore_case: true,
                                limit: None,
                            })
                            .await
                    },
                    move |this, result, cx| {
                        this.with_files(cx, |m| m.apply_search_content(ticket, result));
                    },
                ),
            }
        }
    }

    /// What follows any change to the model: the parsed markdown and the image follow the open
    /// document, a new text starts to be highlighted, a requested line is scrolled to.
    fn files_changed(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        let doc = tab.model.doc().cloned();
        if tab.shown.as_deref() != doc.as_ref().map(|d| d.path.as_str()) {
            tab.shown = doc.as_ref().map(|d| d.path.clone());
            tab.code_scroll = UniformListScrollHandle::new();
        }
        let mut highlight = None;
        if let Some(doc) = &doc {
            match &doc.state {
                DocState::Text(text) => {
                    let key = (doc.path.clone(), text.hash.clone());
                    if doc.is_markdown() && tab.md_for.as_ref() != Some(&key) {
                        tab.md.set_text(&text.text);
                        tab.md_for = Some(key.clone());
                    }
                    if text.highlight == Highlight::Pending
                        && tab.highlighting.as_ref() != Some(&key)
                    {
                        tab.highlighting = Some(key);
                        highlight = Some((doc.path.clone(), text.hash.clone(), text.text.clone()));
                    }
                }
                DocState::Image { bytes, mime }
                    if tab
                        .image
                        .as_ref()
                        .is_none_or(|(old, _)| !Arc::ptr_eq(old, bytes)) =>
                {
                    let format = ImageFormat::from_mime_type(mime).unwrap_or(ImageFormat::Png);
                    tab.image = Some((
                        bytes.clone(),
                        Arc::new(gpui::Image::from_bytes(format, bytes.to_vec())),
                    ));
                }
                _ => {}
            }
        }
        if let Some(line) = tab.model.take_jump() {
            tab.code_scroll
                .scroll_to_item(line.saturating_sub(1) as usize, ScrollStrategy::Center);
        }
        if let Some((path, hash, text)) = highlight {
            self.highlight(path, hash, text, cx);
        }
        cx.notify();
    }

    /// Highlights `text` on a background task and puts the answer in the model.
    fn highlight(
        &mut self,
        path: String,
        hash: Option<String>,
        text: Arc<str>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let source_path = path.clone();
            let doc: Option<Arc<HighlightedDocument>> = cx
                .background_spawn(async move {
                    reno_syntax::highlight(HighlightRequest {
                        source: &text,
                        path: Some(&source_path),
                        fence_tag: None,
                    })
                    .ok()
                    .map(Arc::new)
                })
                .await;
            this.update(cx, |screen, cx| {
                screen.with_files(cx, |m| m.apply_highlight(&path, hash.as_deref(), doc));
            })
            .ok();
        })
        .detach();
    }

    // --- watching -----------------------------------------------------------------------------

    fn start_watch(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.store.read(cx).client() else {
            return;
        };
        let run = self.run.clone();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<WatchMsg>();
        let pump = Tokio::spawn(cx, async move {
            match client.watch_fs(run).await {
                Err(error) => {
                    tx.unbounded_send(WatchMsg::Failed(match fail_of(&error) {
                        Fail::Unavailable(m) | Fail::NotFound(m) | Fail::Other(m) => m,
                        Fail::TooLarge => "too many".to_owned(),
                    }))
                    .ok();
                }
                Ok(mut stream) => {
                    tx.unbounded_send(WatchMsg::Started).ok();
                    while let Some(item) = stream.next().await {
                        let msg = match item {
                            Ok(change) => WatchMsg::Item(change),
                            Err(error) => {
                                tx.unbounded_send(WatchMsg::Failed(error.to_string())).ok();
                                return;
                            }
                        };
                        if tx.unbounded_send(msg).is_err() {
                            return;
                        }
                    }
                    tx.unbounded_send(WatchMsg::Ended).ok();
                }
            }
        });
        let reader = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                if this
                    .update(cx, |screen, cx| screen.watch_message(msg, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        if let Some(tab) = self.files.as_mut() {
            tab.watch = Some(Watch {
                _pump: pump,
                _reader: reader,
            });
        }
    }

    fn watch_message(&mut self, msg: WatchMsg, cx: &mut Context<Self>) {
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        if tab.watch.is_none() {
            // Hidden while this was on its way.
            return;
        }
        let mut requests = Vec::new();
        match msg {
            WatchMsg::Started => tab.model.set_live(Live::On),
            WatchMsg::Item(change) => requests = tab.model.apply_change(&change),
            WatchMsg::Failed(why) => {
                tab.model.set_live(Live::Unavailable(why));
                tab.watch = None;
            }
            WatchMsg::Ended => {
                tab.model
                    .set_live(Live::Unavailable("the connection ended".to_owned()));
                tab.watch = None;
            }
        }
        self.run_files(requests, cx);
        self.files_changed(cx);
    }

    // --- commands -----------------------------------------------------------------------------

    /// A click on a tree row.
    fn files_click(&mut self, path: &str, is_dir: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        let reqs = tab.model.click(path, is_dir);
        self.run_files(reqs, cx);
        self.files_changed(cx);
    }

    /// Opens `path` in the Files tab (and shows the tab): from a search result, from Review.
    pub(crate) fn open_file_at(
        &mut self,
        path: &str,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_surface(Surface::Files, window, cx);
        // The tab is made when it first shows; it may not be yet in a window that has not drawn.
        self.panel_shown = true;
        self.sync_files(cx);
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        let reqs = tab.model.open_path(path, line);
        self.run_files(reqs, cx);
        self.files_changed(cx);
    }

    fn files_search_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        if !tab.model.set_search_text(text) {
            return;
        }
        tab.search_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |screen, cx| screen.commit_files_search(cx))
                .ok();
        }));
        cx.notify();
    }

    fn commit_files_search(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        tab.search_timer = None;
        let reqs = tab.model.commit_search();
        self.run_files(reqs, cx);
        cx.notify();
    }

    fn toggle_files_search_mode(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.files.as_mut() else {
            return;
        };
        tab.model.toggle_search_mode();
        self.commit_files_search(cx);
    }

    /// Types `text` in the search box, as a person would (screenshots, reviews).
    pub(crate) fn type_files_search(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = &self.files {
            tab.search
                .update(cx, |field, cx| field.fill(text, window, cx));
        }
    }

    // --- test seams ---------------------------------------------------------------------------

    #[cfg(test)]
    pub(crate) fn files_model(&self) -> Option<&FilesModel> {
        self.files.as_ref().map(|t| &t.model)
    }

    #[cfg(test)]
    pub(crate) fn files_search_field(&self) -> Option<&Entity<Composer>> {
        self.files.as_ref().map(|t| &t.search)
    }

    #[cfg(test)]
    pub(crate) fn files_scroll_tree_to_end(&self) {
        if let Some(tab) = &self.files {
            tab.tree_scroll.scroll_to_bottom();
        }
    }

    // --- drawing ------------------------------------------------------------------------------

    /// The page of the Files tab.
    pub(super) fn files_body(
        &mut self,
        theme: &Theme,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = self.files_content(theme, width, window, cx);
        div()
            .id("files-tab")
            .debug_selector(|| "files-tab".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn files_content(
        &mut self,
        theme: &Theme,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let note = |id: &'static str, title: &str, text: String| {
            page_note(theme, id, title, text).into_any_element()
        };
        if !self.store.read(cx).supports_files() {
            return note(
                "files-unsupported",
                "Files need a newer daemon",
                "This daemon does not serve files. Update it to browse the worktree.".into(),
            );
        }
        let Some(tab) = self.files.as_ref() else {
            return note(
                "files-waiting",
                "Files",
                "Waiting for the daemon\u{2026}".into(),
            );
        };
        if let Some(reason) = tab.model.unavailable() {
            return note(
                "files-unavailable",
                "Files are not available",
                reason.to_owned(),
            );
        }
        let narrow = width < NARROW_BELOW;
        let viewer_only = narrow && tab.model.show_viewer;
        let tree = self.files_tree_pane(theme, window, cx);
        let viewer = self.files_viewer_pane(theme, narrow, window, cx);
        let tree_w = (width * TREE_SHARE).round();
        let row = div().flex_1().min_h_0().flex();
        if narrow {
            row.child(if viewer_only {
                viewer
            } else {
                div().size_full().child(tree).into_any_element()
            })
        } else {
            row.child(
                div()
                    .flex_none()
                    .w(px(tree_w))
                    .h_full()
                    .border_r_1()
                    .border_color(theme.hairline)
                    .child(tree),
            )
            .child(div().flex_1().min_w_0().h_full().child(viewer))
        }
        .into_any_element()
    }

    /// The left side: the search box, the status line, and the tree or the search results.
    fn files_tree_pane(
        &mut self,
        theme: &Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(tab) = self.files.as_ref() else {
            return div().into_any_element();
        };
        let model = &tab.model;
        let mode = model.search.mode();
        let seg = |id: &'static str, label: &'static str, on: SearchMode| {
            let active = mode == on;
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .h(px(22.0))
                .px(px(9.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(active, |d| {
                    d.bg(theme.text.opacity(0.08)).text_color(theme.text)
                })
                .when(!active, |d| d.text_color(theme.muted))
                .child(label)
                .when(
                    mode != on && !model.search.text().trim_start().starts_with('>'),
                    |d| d.on_click(cx.listener(|this, _, _, cx| this.toggle_files_search_mode(cx))),
                )
        };
        let search = div()
            .flex_none()
            .px(px(8.0))
            .pt(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .id("files-search")
                    .debug_selector(|| "files-search".to_owned())
                    .child(tab.search.clone()),
            )
            .child(
                div()
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(seg("files-mode-names", "Names", SearchMode::Files))
                    .child(seg("files-mode-text", "Text", SearchMode::Content)),
            );
        let status = self.files_status_line(theme);
        let body = if model.search.is_active() {
            self.files_results(theme, cx)
        } else {
            self.files_tree(theme, cx)
        };
        div()
            .id("files-tree-pane")
            .debug_selector(|| "files-tree-pane".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .child(search)
            .child(status)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// The branch and the git state, and the warning when live updates are down.
    fn files_status_line(&self, theme: &Theme) -> AnyElement {
        let Some(tab) = self.files.as_ref() else {
            return div().into_any_element();
        };
        let model = &tab.model;
        let git = &model.git;
        let left = match (&git.branch, git.repo, git.loaded) {
            (Some(branch), _, _) => {
                let changed = git.changed();
                if changed == 0 {
                    branch.clone()
                } else {
                    format!("{branch} \u{b7} {changed} changed")
                }
            }
            (None, false, true) => "not a git repository".to_owned(),
            _ => String::new(),
        };
        div()
            .flex_none()
            .h(px(26.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(11.5))
            .text_color(theme.faint)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(MONO)
                    .child(SharedString::from(left)),
            )
            .child(div().flex_1())
            .when(model.live.is_down(), |d| {
                d.child(
                    div()
                        .id("files-live-down")
                        .debug_selector(|| "files-live-down".to_owned())
                        .flex_none()
                        .text_color(theme.warning)
                        .child("Live updates unavailable"),
                )
            })
            .into_any_element()
    }

    fn files_tree(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.files.as_ref() else {
            return div().into_any_element();
        };
        let rows: Rc<Vec<Row>> = Rc::new(tab.model.rows());
        let count = rows.len();
        let theme = *theme;
        uniform_list(
            "files-tree",
            count,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| rows.get(ix).cloned())
                    .map(|row| this.tree_row(&theme, &row, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&tab.tree_scroll)
        .size_full()
        .into_any_element()
    }

    fn tree_row(&self, theme: &Theme, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Entry(e) => {
                let (path, is_dir) = (e.path.clone(), e.is_dir);
                let selector = format!("files-row-{}", e.path);
                let hover = theme.text.opacity(0.05);
                let (deco_color, deco_letter) = match e.deco {
                    Some(d) => (Some(deco_color(theme, d)), Some(d.letter())),
                    None => (None, None),
                };
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .h(px(ROW))
                    .w_full()
                    .pl(px(8.0 + e.depth as f32 * 14.0))
                    .pr(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .when(e.selected, |d| d.bg(theme.text.opacity(0.08)))
                    .when(!e.selected, |d| d.hover(move |s| s.bg(hover)))
                    .when(e.ignored, |d| d.opacity(0.5))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.files_click(&path, is_dir, cx);
                    }))
                    .child(if e.is_dir {
                        icon(
                            if e.expanded {
                                Icon::PhCaretDown
                            } else {
                                Icon::PhCaretRight
                            },
                            11.0,
                            theme.muted,
                        )
                    } else {
                        // The file glyph is in the chevron's column, so names line up.
                        icon(Icon::File, 12.0, theme.faint)
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(13.0))
                            .when(e.is_dir, |d| d.font_weight(FontWeight::MEDIUM))
                            .when_some(deco_color, |d, color| d.text_color(color))
                            .child(SharedString::from(e.name.clone())),
                    )
                    .when(e.dirty, |d| {
                        d.child(kit::dot(theme.warning.opacity(0.8), 6.0))
                    })
                    .when_some(deco_letter.zip(deco_color), |d, (letter, color)| {
                        d.child(
                            div()
                                .flex_none()
                                .w(px(12.0))
                                .font_family(MONO)
                                .text_size(px(11.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(color)
                                .child(letter.to_string()),
                        )
                    })
                    .into_any_element()
            }
            Row::Note { dir, depth, note } => {
                let pad = px(8.0 + *depth as f32 * 14.0 + 17.0);
                let base = div()
                    .h(px(ROW))
                    .w_full()
                    .pl(pad)
                    .pr(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .text_color(theme.faint);
                match note {
                    Note::Loading => base.child("Loading\u{2026}").into_any_element(),
                    Note::Empty => base.child("Empty folder").into_any_element(),
                    Note::Failed(message) => {
                        let dir = dir.clone();
                        let selector = format!("files-retry-{dir}");
                        base.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.danger)
                                .child(SharedString::from(message.clone())),
                        )
                        .child(
                            kit::line_button(theme, selector, None, "Try again", Tone::Quiet, true)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let Some(tab) = this.files.as_mut() else {
                                        return;
                                    };
                                    let reqs = tab.model.retry(&dir);
                                    this.run_files(reqs, cx);
                                    this.files_changed(cx);
                                })),
                        )
                        .into_any_element()
                    }
                    Note::More { paging } => {
                        let dir = dir.clone();
                        let selector = format!("files-more-{dir}");
                        base.child(
                            kit::line_button(
                                theme,
                                selector,
                                None,
                                if *paging {
                                    "Loading\u{2026}"
                                } else {
                                    "Show more"
                                },
                                Tone::Quiet,
                                !*paging,
                            )
                            .when(!*paging, |b| {
                                b.on_click(cx.listener(move |this, _, _, cx| {
                                    let Some(tab) = this.files.as_mut() else {
                                        return;
                                    };
                                    let reqs = tab.model.load_more(&dir);
                                    this.run_files(reqs, cx);
                                    this.files_changed(cx);
                                }))
                            }),
                        )
                        .into_any_element()
                    }
                }
            }
        }
    }

    fn files_results(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.files.as_ref() else {
            return div().into_any_element();
        };
        let note = |text: String| {
            div()
                .id("files-results-note")
                .debug_selector(|| "files-results-note".to_owned())
                .px(px(16.0))
                .pt(px(14.0))
                .text_size(px(12.5))
                .text_color(theme.muted)
                .child(SharedString::from(text))
                .into_any_element()
        };
        // One line for each: (path, line, the text, whether the text is code).
        let items: Vec<(String, Option<u32>, Option<String>)> = match &tab.model.search.results {
            Results::Idle | Results::Loading => return note("Searching\u{2026}".to_owned()),
            Results::Failed(message) => return note(message.clone()),
            Results::Files { files, .. } => {
                if files.is_empty() {
                    return note("No file matches.".to_owned());
                }
                files.iter().map(|f| (f.path.clone(), None, None)).collect()
            }
            Results::Content { matches, .. } => {
                if matches.is_empty() {
                    return note("No line matches.".to_owned());
                }
                matches
                    .iter()
                    .map(|m| (m.path.clone(), Some(m.line), Some(m.text.trim().to_owned())))
                    .collect()
            }
        };
        let truncated = matches!(
            &tab.model.search.results,
            Results::Files {
                truncated: true,
                ..
            } | Results::Content {
                truncated: true,
                ..
            }
        );
        let items = Rc::new(items);
        let count = items.len();
        let theme = *theme;
        div()
            .id("files-results")
            .debug_selector(|| "files-results".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .when(truncated, |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(px(16.0))
                        .py(px(4.0))
                        .text_size(px(11.5))
                        .text_color(theme.faint)
                        .child("Showing the best matches only."),
                )
            })
            .child(
                uniform_list(
                    "files-results-list",
                    count,
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        range
                            .filter_map(|ix| items.get(ix).map(|item| (ix, item.clone())))
                            .map(|(ix, (path, line, text))| {
                                this.result_row(&theme, ix, &path, line, text.as_deref(), cx)
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&tab.results_scroll)
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    fn result_row(
        &self,
        theme: &Theme,
        ix: usize,
        path: &str,
        line: Option<u32>,
        text: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
        let selector = format!("files-result-{ix}");
        let hover = theme.text.opacity(0.05);
        let target = path.to_owned();
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .h(px(RESULT))
            .w_full()
            .px(px(14.0))
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(2.0))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_file_at(&target, line, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(6.0))
                    .text_size(px(13.0))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(name.to_owned())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.faint)
                            .child(SharedString::from(match line {
                                Some(line) => format!(
                                    "{dir}{}:{line}",
                                    if dir.is_empty() { "" } else { "  " }
                                ),
                                None => dir.to_owned(),
                            })),
                    ),
            )
            .when_some(text, |d, text| {
                d.child(
                    div()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(11.5))
                        .text_color(theme.muted)
                        .child(SharedString::from(text.to_owned())),
                )
            })
            .into_any_element()
    }

    // --- the viewer ---------------------------------------------------------------------------

    fn files_viewer_pane(
        &mut self,
        theme: &Theme,
        narrow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(tab) = self.files.as_ref() else {
            return div().into_any_element();
        };
        let model = &tab.model;
        let header = self.files_viewer_header(theme, narrow, cx);
        let body: AnyElement = match model.doc() {
            None => viewer_note(theme, "files-doc-none", "Select a file to read it.", None),
            Some(doc) => match &doc.state {
                DocState::Loading => div()
                    .id("files-doc-loading")
                    .debug_selector(|| "files-doc-loading".to_owned())
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(kit::spinner("files-doc-spinner", 18.0, theme.faint))
                    .into_any_element(),
                DocState::Text(text) => {
                    if doc.is_markdown() && doc.view == View::Preview {
                        div()
                            .id("files-doc-markdown")
                            .debug_selector(|| "files-doc-markdown".to_owned())
                            .size_full()
                            .overflow_y_scroll()
                            .px(px(22.0))
                            .py(px(16.0))
                            .child(
                                div()
                                    .max_w(px(720.0))
                                    .text_size(px(14.0))
                                    .line_height(px(21.0))
                                    .child(markdown_view::blocks_with(
                                        tab.md.tree(),
                                        theme,
                                        markdown_view::Host::preview(),
                                    )),
                            )
                            .into_any_element()
                    } else {
                        self.files_code(theme, text, doc.line, window, cx)
                    }
                }
                DocState::Binary { size } => viewer_note(
                    theme,
                    "files-doc-binary",
                    "This is a binary file.",
                    Some(format!("{} \u{b7} no preview", size_label(*size))),
                ),
                DocState::TooLarge { size, limit } => viewer_note(
                    theme,
                    "files-doc-too-large",
                    "This file is too large to show.",
                    Some(match size {
                        Some(size) => {
                            format!(
                                "{} \u{b7} the limit is {}",
                                size_label(*size),
                                size_label(*limit)
                            )
                        }
                        None => format!("the limit is {}", size_label(*limit)),
                    }),
                ),
                DocState::Image { .. } => match &tab.image {
                    Some((_, image)) => div()
                        .id("files-doc-image")
                        .debug_selector(|| "files-doc-image".to_owned())
                        .size_full()
                        .p(px(16.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            img(image.clone())
                                .object_fit(ObjectFit::Contain)
                                .max_w_full()
                                .max_h_full(),
                        )
                        .into_any_element(),
                    None => div().into_any_element(),
                },
                DocState::Unavailable(message) => {
                    viewer_note(theme, "files-doc-unavailable", message, None)
                }
                DocState::Failed(message) => div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(10.0))
                    .px(px(22.0))
                    .pt(px(28.0))
                    .child(
                        div()
                            .id("files-doc-failed")
                            .debug_selector(|| "files-doc-failed".to_owned())
                            .text_size(px(13.5))
                            .text_color(theme.danger)
                            .child(SharedString::from(message.clone())),
                    )
                    .child(
                        kit::pill_button(theme, "files-doc-retry", "Try again", Tone::Quiet)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(tab) = this.files.as_mut() else {
                                    return;
                                };
                                let reqs = tab.model.reload_doc();
                                this.run_files(reqs, cx);
                                this.files_changed(cx);
                            })),
                    )
                    .into_any_element(),
            },
        };
        div()
            .id("files-viewer")
            .debug_selector(|| "files-viewer".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn files_viewer_header(
        &self,
        theme: &Theme,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(tab) = self.files.as_ref() else {
            return div().into_any_element();
        };
        let model = &tab.model;
        let doc = model.doc();
        let meta = doc.and_then(|doc| match &doc.state {
            DocState::Text(t) => {
                // The common case (UTF-8, LF) is not worth the room; anything else is.
                let mut parts = vec![format!("{} lines", t.lines()), size_label(t.size)];
                if t.encoding != "utf-8" {
                    parts.push(t.encoding.clone());
                }
                match t.line_ending {
                    reno_proto::LineEnding::Crlf => parts.push("CRLF".to_owned()),
                    reno_proto::LineEnding::Mixed => parts.push("mixed endings".to_owned()),
                    _ => {}
                }
                Some(parts.join(" \u{b7} "))
            }
            _ => None,
        });
        let path = doc.map(|d| d.path.clone());
        let markdown = doc.filter(|d| d.is_markdown() && matches!(d.state, DocState::Text(_)));
        div()
            .id("files-viewer-header")
            .debug_selector(|| "files-viewer-header".to_owned())
            .flex_none()
            .h(px(36.0))
            .pl(px(if narrow { 6.0 } else { 14.0 }))
            .pr(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.hairline)
            .when(narrow, |d| {
                d.child(
                    kit::icon_button(theme, "files-back", Icon::Back, 26.0).on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(tab) = this.files.as_mut() {
                                tab.model.back();
                            }
                            cx.notify();
                        },
                    )),
                )
            })
            .child(match &path {
                Some(path) => {
                    let (dir, name) = path
                        .rsplit_once('/')
                        .map_or(("", path.as_str()), |(d, n)| (d, n));
                    let dir = if dir.is_empty() {
                        String::new()
                    } else {
                        format!("{dir}/")
                    };
                    let copy_path = path.clone();
                    div()
                        .id("files-doc-path")
                        .debug_selector(|| "files-doc-path".to_owned())
                        .min_w_0()
                        .flex()
                        .text_size(px(13.0))
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            copy(cx, copy_path.clone(), "The path is on the clipboard.");
                        }))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.faint)
                                .child(SharedString::from(dir)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(SharedString::from(name.to_owned())),
                        )
                        .into_any_element()
                }
                None => div()
                    .text_size(px(13.0))
                    .text_color(theme.faint)
                    .child("No file open")
                    .into_any_element(),
            })
            .when(model.in_place, |d| {
                d.child(
                    div()
                        .id("files-not-worktree")
                        .debug_selector(|| "files-not-worktree".to_owned())
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(theme.warning)
                        .child("(not a worktree)"),
                )
            })
            .child(div().flex_1())
            .when_some(meta, |d, meta| {
                d.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(MONO)
                        .text_size(px(11.0))
                        .text_color(theme.faint)
                        .child(SharedString::from(meta)),
                )
            })
            .when_some(markdown.map(|d| d.view), |d, view| {
                let seg = |id: &'static str, label: &'static str, on: View| {
                    let active = view == on;
                    div()
                        .id(id)
                        .debug_selector(move || id.to_owned())
                        .h(px(22.0))
                        .px(px(9.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .when(active, |d| {
                            d.bg(theme.text.opacity(0.08)).text_color(theme.text)
                        })
                        .when(!active, |d| d.text_color(theme.muted))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(tab) = this.files.as_mut() {
                                tab.model.set_view(on);
                            }
                            cx.notify();
                        }))
                };
                d.child(
                    div()
                        .flex_none()
                        .flex()
                        .gap(px(2.0))
                        .child(seg("files-md-preview", "Preview", View::Preview))
                        .child(seg("files-md-source", "Source", View::Source)),
                )
            })
            .into_any_element()
    }

    /// The text with line numbers, from the highlight when there is one.
    fn files_code(
        &self,
        theme: &Theme,
        text: &crate::vm::files::Text,
        marked: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(tab) = self.files.as_ref() else {
            return div().into_any_element();
        };
        let lines = text.lines();
        let digits = lines.to_string().len().max(2);
        let gutter = px(digits as f32 * 7.4 + 22.0);
        let spans: Option<Arc<HighlightedDocument>> = match &text.highlight {
            Highlight::Ready(doc) => Some(doc.clone()),
            _ => None,
        };
        let starts_text = text.clone();
        let base = window.text_style();
        let theme = *theme;
        let marked_ix = marked.map(|l| l.saturating_sub(1) as usize);
        let cut = text.truncated.then(|| {
            format!(
                "Showing the first {} of {}.",
                size_label(FilesModel::READ_LIMIT as u64),
                size_label(text.size)
            )
        });
        let list = uniform_list(
            "files-code",
            lines,
            move |range: Range<usize>, _window: &mut Window, _cx: &mut gpui::App| {
                range
                    .map(|ix| {
                        let line = starts_text.line(ix);
                        let body: AnyElement = match spans.as_ref().and_then(|d| d.lines.get(ix)) {
                            Some(line_spans) if !line.is_empty() => {
                                StyledText::new(SharedString::from(line.to_owned()))
                                    .with_runs(markdown_view::line_runs(
                                        line, line_spans, &theme, &base,
                                    ))
                                    .into_any_element()
                            }
                            _ => div()
                                .child(SharedString::from(line.to_owned()))
                                .into_any_element(),
                        };
                        div()
                            .h(px(LINE))
                            .flex()
                            .items_center()
                            .when(marked_ix == Some(ix), |d| d.bg(theme.accent.opacity(0.14)))
                            .child(
                                div()
                                    .flex_none()
                                    .w(gutter)
                                    .pr(px(10.0))
                                    .text_right()
                                    .text_color(theme.faint)
                                    .child(SharedString::from((ix + 1).to_string())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .pr(px(16.0))
                                    .child(body),
                            )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&tab.code_scroll)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(text.longest))
        .flex_1()
        .min_h_0();
        let _ = cx;
        div()
            .id("files-doc-text")
            .debug_selector(|| "files-doc-text".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .font_family(MONO)
            .text_size(px(12.5))
            .line_height(px(LINE))
            .when_some(cut, |d, cut| {
                d.child(
                    div()
                        .id("files-doc-cut")
                        .debug_selector(|| "files-doc-cut".to_owned())
                        .flex_none()
                        .px(px(14.0))
                        .py(px(5.0))
                        .font_family(crate::ui::kit::SANS)
                        .text_size(px(12.0))
                        .text_color(theme.warning)
                        .child(SharedString::from(cut)),
                )
            })
            .child(list)
            .into_any_element()
    }
}

/// The colour of a git letter.
fn deco_color(theme: &Theme, deco: Deco) -> Hsla {
    match deco {
        Deco::Added | Deco::Untracked => theme.success,
        Deco::Modified => theme.warning,
        Deco::Renamed => theme.muted,
        Deco::Deleted | Deco::Conflicted => theme.danger,
    }
}

/// "3.2 KB": bytes in the unit a person reads.
pub(crate) fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A short statement in the middle of the viewer, with an optional second line.
fn viewer_note(theme: &Theme, id: &'static str, text: &str, sub: Option<String>) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(4.0))
        .px(px(22.0))
        .child(
            div()
                .text_size(px(13.5))
                .text_color(theme.muted)
                .child(SharedString::from(text.to_owned())),
        )
        .when_some(sub, |d, sub| {
            d.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(SharedString::from(sub)),
            )
        })
        .into_any_element()
}

/// A whole-tab note: nothing to browse, and why.
fn page_note(theme: &Theme, id: &'static str, title: &str, text: String) -> impl IntoElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .size_full()
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(px(460.0))
                .px(px(28.0))
                .pt(px(44.0))
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.0))
                .child(icon(Icon::Folder, 20.0, theme.muted))
                .child(
                    div()
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(title.to_owned())),
                )
                .child(
                    div()
                        .text_size(px(13.5))
                        .text_color(theme.muted)
                        .child(SharedString::from(text)),
                ),
        )
}
