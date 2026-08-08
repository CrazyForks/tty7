use gpui::{AnyElement, ElementId, ScrollHandle, div, prelude::*};
use gpui_component::scroll::Scrollbar;
use gpui_component::v_flex;

pub(crate) fn with_vertical_scrollbar(
    id: impl Into<ElementId>,
    scroll_area: impl IntoElement,
    handle: &ScrollHandle,
) -> AnyElement {
    v_flex()
        .relative()
        .flex_1()
        .min_h_0()
        // Stretching would size this the same, but not *definitely*: a `w_full`
        // inside the scroll area would then have no width to be a percentage
        // of, and would fall back to its content. That is how the settings
        // reading column lost its 640px cap on the Chinese page — one wide row
        // measured wider, and every row followed it.
        .w_full()
        .child(scroll_area)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                // No `scrollbar_show` override: it falls back to
                // `cx.theme().scrollbar_show`, which `apply_theme` derives from
                // `should_auto_hide_scrollbars()` — the OS "show scroll bars"
                // preference. Pinning it here would take that choice away from
                // everyone who asked for always-visible bars.
                .child(Scrollbar::vertical(handle).id(id)),
        )
        .into_any_element()
}
