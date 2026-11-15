//! The plain model: state plus a pure `apply`. GPUI only wraps it in an `Entity`.

use gpui::{Context, Task};

/// Everything that can happen to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The background ticker reached this count.
    Tick(u64),
    /// Reset to the initial state.
    Reset,
}

/// GPUI-free state mirror.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) count: u64,
    pub(crate) resets: u32,
}

impl State {
    /// The one pure transition function.
    pub(crate) fn apply(&mut self, event: Event) {
        match event {
            Event::Tick(n) => self.count = n,
            Event::Reset => {
                self.count = 0;
                self.resets += 1;
            }
        }
    }
}

/// The entity views observe. It owns the bridge task, so dropping it stops the ticker.
#[derive(Debug, Default)]
pub(crate) struct Model {
    pub(crate) state: State,
    bridge: Option<Task<()>>,
}

impl Model {
    pub(crate) fn dispatch(&mut self, event: Event, cx: &mut Context<Self>) {
        self.state.apply(event);
        cx.notify();
    }

    pub(crate) fn hold_bridge(&mut self, task: Task<()>) {
        self.bridge = Some(task);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{AppContext, TestAppContext};

    use super::*;

    #[test]
    fn apply_is_pure_and_total() {
        let mut state = State::default();
        state.apply(Event::Tick(3));
        assert_eq!(state.count, 3);
        state.apply(Event::Reset);
        assert_eq!(
            state,
            State {
                count: 0,
                resets: 1
            }
        );
    }

    #[gpui::test]
    fn dispatch_applies_and_notifies_observers(cx: &mut TestAppContext) {
        let model = cx.new(|_| Model::default());
        let notified = Rc::new(Cell::new(0));
        let seen = notified.clone();
        let observed = model.clone();
        let _subscription = cx.update(|cx| {
            cx.observe(&model, move |_, cx| {
                seen.set(seen.get() + 1);
                assert_eq!(observed.read(cx).state.count, 7);
            })
        });

        model.update(cx, |model, cx| model.dispatch(Event::Tick(7), cx));

        assert_eq!(notified.get(), 1);
        assert_eq!(model.read_with(cx, |model, _| model.state.count), 7);
    }
}
