use crate::{centered_message, theme, ReviewApp};
use gpui::{div, prelude::*, px, Context};

impl ReviewApp {
    pub(crate) fn render_tracker_sidebar(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_r_1()
            .border_color(theme::surface0())
            .text_size(px(12.))
            .child(centered_message("Tracker".into(), theme::overlay0()))
    }

    pub(crate) fn render_tracker_pane(&self, _cx: &mut Context<Self>) -> gpui::AnyElement {
        centered_message("Tracker".into(), theme::overlay0())
    }
}
