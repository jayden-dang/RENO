//! Views and the small design-system pieces they use.

pub(crate) mod bar;
pub(crate) mod composer;
pub(crate) mod draft32;
pub(crate) mod field;
pub(crate) mod icon;
pub(crate) mod kit;
pub(crate) mod markdown_view;
pub(crate) mod motion;
pub(crate) mod notice;
pub(crate) mod overlay;
pub(crate) mod page;
pub(crate) mod palette;
pub(crate) mod ring;
pub(crate) mod running_pill;
pub(crate) mod sidebar;
pub(crate) mod terminal_view;
pub(crate) mod text_area;
pub(crate) mod text_model;
pub(crate) mod theme;
pub(crate) mod toast;

#[cfg(test)]
mod tests {
    use gpui::prelude::*;
    use gpui::{Context, TestAppContext, Window, div, px};

    use super::notice::{NoticeChipIcon, NoticeTone, notice_chip};
    use super::running_pill::running_pill;
    use super::theme::{Appearance, Theme};

    #[derive(Debug)]
    struct Gallery(Theme);

    impl Render for Gallery {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let theme = self.0;
            div()
                .size(px(400.0))
                .child(running_pill("pill", 3, theme.accent, "Menlo"))
                .child(running_pill("capped", 120, theme.accent, "Menlo"))
                .child(notice_chip(
                    &theme,
                    NoticeTone::Danger,
                    "Failed",
                    "exit status 1",
                    NoticeChipIcon::Tile,
                ))
                .child(notice_chip(
                    &theme,
                    NoticeTone::Warning,
                    "Held",
                    "needs approval",
                    NoticeChipIcon::Plain,
                ))
        }
    }

    #[gpui::test]
    fn pill_and_notice_draw_in_a_test_window(cx: &mut TestAppContext) {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let theme = Theme::for_appearance(appearance);
            let (_view, cx) = cx.add_window_view(|_, _| Gallery(theme));
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
            });
        }
    }
}
