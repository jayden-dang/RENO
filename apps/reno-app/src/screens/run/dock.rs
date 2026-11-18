//! The dock under the transcript: the steer input, in the chat's column.

use gpui::{IntoElement, div, prelude::*, px};

use super::{COLUMN, RunScreen};

impl RunScreen {
    pub(super) fn dock(&self) -> impl IntoElement {
        div()
            .flex_none()
            .w_full()
            .max_w(px(COLUMN))
            .pb(px(20.0))
            .pt(px(6.0))
            .child(self.steer.clone())
    }
}
