//! Choosing a folder on this Mac: the native open panel, directories only, one selection.
//!
//! [`FolderPicker`] is injected (ARCH-16), as the URL opener and Terminal are. The real one asks
//! the platform's panel. Tests install a recording fake, and with none installed a test that
//! reaches [`pick`] panics, so a test can never open the real panel.

use std::path::PathBuf;

use futures::channel::oneshot;
use gpui::{App, PathPromptOptions};

/// What the panel answers: the chosen folders, or `None` when it was cancelled.
pub(crate) type Chosen = oneshot::Receiver<Answer>;

/// The panel's answer, or why it could not be shown.
pub(crate) type Answer = gpui::Result<Option<Vec<PathBuf>>>;

/// Asks the person for a folder.
pub(crate) trait FolderPicker {
    fn pick(&self, cx: &App) -> Chosen;
}

/// The real panel.
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct MacPicker;

impl FolderPicker for MacPicker {
    fn pick(&self, cx: &App) -> Chosen {
        cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Import".into()),
        })
    }
}

#[cfg(test)]
struct Handle(std::rc::Rc<dyn FolderPicker>);

#[cfg(test)]
impl gpui::Global for Handle {}

/// Installs the picker the setup uses. The app installs none: it uses the platform's.
#[cfg(test)]
pub(crate) fn install(cx: &mut App, picker: std::rc::Rc<dyn FolderPicker>) {
    cx.set_global(Handle(picker));
}

/// Opens the panel. In a test with no picker installed this panics.
pub(crate) fn pick(cx: &App) -> Chosen {
    #[cfg(test)]
    match cx.try_global::<Handle>() {
        Some(handle) => handle.0.pick(cx),
        None => panic!("a test reached the real folder panel: install a fake with `install`"),
    }
    #[cfg(not(test))]
    MacPicker.pick(cx)
}

/// The folder a finished panel chose: the first one, if any.
pub(crate) fn first(answer: Answer) -> Option<PathBuf> {
    answer
        .ok()
        .flatten()
        .and_then(|paths| paths.into_iter().next())
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// Answers with the folders it was given (or a cancel), and counts the times it was asked.
    #[derive(Default)]
    pub(crate) struct Recording {
        pub(crate) asked: Cell<usize>,
        pub(crate) answer: RefCell<Option<PathBuf>>,
    }

    impl FolderPicker for Recording {
        fn pick(&self, _: &App) -> Chosen {
            self.asked.set(self.asked.get() + 1);
            let (tx, rx) = oneshot::channel();
            tx.send(Ok(self.answer.borrow().clone().map(|p| vec![p])))
                .ok();
            rx
        }
    }
}
