//! Opening a text box on a value the user is about to replace.
//!
//! A box that comes up with something already in it — a rename, a find bar, a
//! prompt carrying a suggested name — only behaves if the caret treats that
//! text as a *value* rather than as text to write around.
//! `InputState::default_value` leaves the caret at offset 0, so the first
//! keystroke prepends ("beta" over "alpha" gives "betaalpha"); `set_value`
//! leaves it at the end, so it appends. Both are wrong for something you are
//! replacing, and every find bar and rename box on the platform answers the
//! same way: select it.

use gpui::{AppContext as _, Context, Entity, Window};
use gpui_component::input::{InputState, SelectAll};

/// How many frames to keep trying for. Two is enough everywhere in the app;
/// the rest is headroom for a surface that takes an extra frame to lay the box
/// out, and the loop stops the moment the selection lands either way.
const ATTEMPTS: u8 = 4;

/// Select everything in `input` as soon as it has been drawn.
///
/// `InputState::select_all` is `pub(super)` in the UI crate, so this goes
/// through the action the input itself binds ⌘A to, aimed at the box's own
/// focus handle rather than at whatever happens to be focused.
///
/// It has to wait for a frame: actions route along the dispatch tree of the
/// last frame *drawn*, and a box created this turn is not in it — and frame
/// callbacks run before that frame is drawn, not after, so one wait is not
/// always enough. Rather than guess, dispatch and then look at whether it
/// took. That is also why this cannot be unit-tested: the test harness never
/// paints.
pub(crate) fn select_all_when_drawn<T: 'static>(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    if input.read(cx).value().is_empty() {
        return;
    }
    try_select(input.clone(), ATTEMPTS, window);
}

fn try_select(input: Entity<InputState>, left: u8, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        let handle = gpui::Focusable::focus_handle(input.read(cx), cx);
        handle.dispatch_action(&SelectAll, window, cx);
        if input.read(cx).selected_range().is_empty() && left > 1 {
            try_select(input, left - 1, window);
        }
    });
    // Registering a callback does not by itself ask for a frame, and a panel
    // that has finished drawing has no other reason to produce one.
    window.refresh();
}

/// A new box holding `value`, with the value selected.
///
/// Focusing is left to the caller: a form opens with one of its boxes focused
/// and the rest merely filled, and tabbing into one of those selects it the
/// same way.
pub(crate) fn filled_box<T: 'static>(
    value: impl Into<String>,
    window: &mut Window,
    cx: &mut Context<T>,
) -> Entity<InputState> {
    let input = cx.new(|cx| InputState::new(window, cx));
    input.update(cx, |state, cx| state.set_value(value.into(), window, cx));
    select_all_when_drawn(&input, window, cx);
    input
}
