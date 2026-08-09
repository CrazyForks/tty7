//! The history section at the foot of the panel.
//!
//! Its job is shape, not text. 260px leaves room for roughly 26 characters
//! beside the lanes, and this repository's commit subjects run to a median of
//! 64 — so what a reader gets here is where the branches are, where they
//! merged, which refs sit where, and how recently anything moved. Reading a
//! message is the commit detail view's job, one click away.
//!
//! That is also VS Code's own reading of a sidebar graph, and it is why the
//! conventional-commit prefix is lifted out into a chip rather than left to
//! eat half the line.
//!
//! # Spike (G7·0)
//!
//! What is below is deliberately a hard-coded three-row figure. It exists to
//! prove the four load-bearing claims of the rendering plan before any of the
//! real data is wired to it: that one absolutely-positioned canvas draws
//! correctly inside a scroll container, that `content_mask` gives usable
//! culling bounds, that the row `div`s underneath still receive hover and
//! click through the canvas above them, and that the height drag feels right.
//! The next commit replaces the fake rows with `CommitPage` and keeps the
//! shape.

use std::cell::Cell as StdCell;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, Context, Corners, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels,
    SharedString, Window, canvas, div, fill, prelude::*, px,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::scm::state::RepoKey;

/// One commit per row. 20px rather than the file list's 24: a graph row has no
/// icon column, and the lane geometry reads better when the vertical pitch is
/// close to the lane pitch.
const GRAPH_ROW_H: f32 = 20.;

/// Horizontal distance between lane centres.
const GRAPH_LANE_W: f32 = 12.;

/// Inset before the first lane centre, so lane 0 is not flush against the
/// panel edge.
const GRAPH_PAD_L: f32 = 6.;

const GRAPH_DOT_R: f32 = 3.;
const GRAPH_LINE_W: f32 = 1.5;

/// Resting height of the history section, and the range the divider drags it
/// through. The maximum is a fraction of the panel rather than a constant:
/// the file list has to keep a usable share of a short window.
const GRAPH_H_DEFAULT: f32 = 220.;
const GRAPH_H_MIN: f32 = 88.;
const GRAPH_H_MAX_RATIO: f32 = 0.65;

/// The divider's grab area, matching `RESIZE_HANDLE_WIDTH` on the other axis.
const GRAPH_HANDLE_H: f32 = 6.;

/// Snap a lane centre to a device pixel *before* the quad is built.
///
/// `paint_quad` snaps the bounds it is given, but it does that to each edge
/// independently: an unsnapped centre makes `[cx - w/2, cx + w/2]` round out
/// to one physical pixel on some rows and two on others, and a column of lines
/// that changes width as it scrolls is the most visible artefact this element
/// can produce. Same reasoning, same shape as `powerline_solid_edge`.
fn snap(x: f32, scale: f32) -> f32 {
    if !scale.is_finite() || scale <= 0. {
        return x;
    }
    (x * scale).round() / scale
}

/// Centre of `lane`, in the canvas's own coordinate space.
fn lane_center_x(lane: u16, scale: f32) -> f32 {
    snap(
        GRAPH_PAD_L + GRAPH_LANE_W * lane as f32 + GRAPH_LANE_W / 2.,
        scale,
    )
}

impl Tty7App {
    /// The history section, when it is expanded and has something to draw.
    ///
    /// Sits below the file list as its own scroll region rather than at the
    /// end of one: the graph pages, and sharing a scroller would mean scrolling
    /// back past hundreds of commits to reach the message box.
    pub(crate) fn render_graph_section(
        &mut self,
        _repo: &RepoKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.scm.graph.expanded {
            return Some(self.graph_header(cx));
        }
        if self.scm.graph.height.get() <= 0. {
            self.scm.graph.height.set(GRAPH_H_DEFAULT);
        }
        let max = (window.viewport_size().height.as_f32() * GRAPH_H_MAX_RATIO).max(GRAPH_H_MIN);
        let height = self.scm.graph.height.get().clamp(GRAPH_H_MIN, max);

        // The spike's figure: a straight lane 0, a branch that opens on row 1
        // and merges back on row 2. Three dots, one elbow each way.
        let mut rows: Vec<(u16, &'static str)> = vec![
            (0, "third commit on the trunk"),
            (1, "a branch opens here"),
            (0, "and merges back in"),
        ];
        // Enough rows that the section actually scrolls, which is the only way
        // to watch the culling window track the viewport.
        for _ in 0..30 {
            rows.push((0, "filler so the section scrolls"));
        }
        let lanes = 2u16;
        let gutter = GRAPH_PAD_L + GRAPH_LANE_W * lanes as f32;
        let scale = window.scale_factor();
        let line = cx.theme().accent;
        let alt = cx.theme().warning;
        let sf = cx.theme().secondary;
        let fg = cx.theme().foreground;

        let painted = Rc::new(StdCell::new(0usize));
        let seen_mask = Rc::new(StdCell::new(0.0f32));
        let clicks = self.scm.graph.selected.clone();

        let body = div()
            .relative()
            .w_full()
            .h(px(rows.len() as f32 * GRAPH_ROW_H))
            .child(
                v_flex().children(
                    rows.iter()
                        .enumerate()
                        .map(|(i, (_, text))| self.graph_spike_row(i, text, cx)),
                ),
            )
            .child(
                canvas(|_, _, _| (), {
                    let rows = rows.clone();
                    let painted = painted.clone();
                    let seen_mask = seen_mask.clone();
                    move |bounds: Bounds<Pixels>, _, window: &mut Window, _| {
                        // Culling: the canvas is as tall as the whole
                        // content, so only the band the mask allows is
                        // worth iterating.
                        let mask = window.content_mask().bounds;
                        seen_mask.set(mask.size.height.as_f32());
                        let top = bounds.origin.y.as_f32();
                        let first = (((mask.origin.y.as_f32() - top) / GRAPH_ROW_H).floor()
                            as isize)
                            .max(0) as usize;
                        let last = ((((mask.origin.y + mask.size.height).as_f32() - top)
                            / GRAPH_ROW_H)
                            .ceil() as isize)
                            .max(0) as usize;
                        let mut n = 0usize;
                        for (i, (lane, _)) in rows.iter().enumerate().skip(first).take(last - first)
                        {
                            let y0 = top + i as f32 * GRAPH_ROW_H;
                            let mid = y0 + GRAPH_ROW_H / 2.;
                            let cx0 = bounds.origin.x.as_f32() + lane_center_x(0, scale);
                            let cx1 = bounds.origin.x.as_f32() + lane_center_x(1, scale);
                            let c = if *lane == 0 { line } else { alt };
                            // Lane 0 runs the full height of every band.
                            window.paint_quad(fill(
                                Bounds::from_corners(
                                    gpui::point(px(cx0 - GRAPH_LINE_W / 2.), px(y0)),
                                    gpui::point(px(cx0 + GRAPH_LINE_W / 2.), px(y0 + GRAPH_ROW_H)),
                                ),
                                line,
                            ));
                            n += 1;
                            // Row 1 opens lane 1 with an elbow, row 2
                            // closes it with the mirror image.
                            if i == 1 {
                                window.paint_quad(fill(
                                    Bounds::from_corners(
                                        gpui::point(px(cx0), px(mid - GRAPH_LINE_W / 2.)),
                                        gpui::point(px(cx1), px(mid + GRAPH_LINE_W / 2.)),
                                    ),
                                    alt,
                                ));
                                window.paint_quad(fill(
                                    Bounds::from_corners(
                                        gpui::point(px(cx1 - GRAPH_LINE_W / 2.), px(mid)),
                                        gpui::point(
                                            px(cx1 + GRAPH_LINE_W / 2.),
                                            px(y0 + GRAPH_ROW_H),
                                        ),
                                    ),
                                    alt,
                                ));
                                n += 2;
                            }
                            if i == 2 {
                                window.paint_quad(fill(
                                    Bounds::from_corners(
                                        gpui::point(px(cx1 - GRAPH_LINE_W / 2.), px(y0)),
                                        gpui::point(px(cx1 + GRAPH_LINE_W / 2.), px(mid)),
                                    ),
                                    alt,
                                ));
                                window.paint_quad(fill(
                                    Bounds::from_corners(
                                        gpui::point(px(cx0), px(mid - GRAPH_LINE_W / 2.)),
                                        gpui::point(px(cx1), px(mid + GRAPH_LINE_W / 2.)),
                                    ),
                                    alt,
                                ));
                                n += 2;
                            }
                            // The node. A rounded quad, not a path: quads
                            // get the SDF's analytic anti-aliasing, and a
                            // path would open a whole render pass.
                            let cxn = if *lane == 0 { cx0 } else { cx1 };
                            window.paint_quad(
                                fill(
                                    Bounds::from_corners(
                                        gpui::point(px(cxn - GRAPH_DOT_R), px(mid - GRAPH_DOT_R)),
                                        gpui::point(px(cxn + GRAPH_DOT_R), px(mid + GRAPH_DOT_R)),
                                    ),
                                    c,
                                )
                                .corner_radii(Corners::all(px(GRAPH_DOT_R))),
                            );
                            n += 1;
                        }
                        painted.set(n);
                    }
                })
                .absolute()
                .top_0()
                .left_0()
                .w(px(gutter))
                .h_full(),
            );

        let scroller = div()
            .id("scm-graph")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scm.graph.scroll)
            .child(body);

        let (backing, handle) = self.graph_resize(max, cx);
        let _ = clicks;
        Some(
            v_flex()
                .relative()
                .flex_none()
                .h(px(height))
                .border_t_1()
                .border_color(cx.theme().border)
                .bg(sf)
                .text_color(fg)
                .child(backing)
                .child(self.graph_header(cx))
                .child(scroller)
                .child(handle)
                .into_any_element(),
        )
    }

    fn graph_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let expanded = self.scm.graph.expanded;
        h_flex()
            .id("scm-graph-header")
            .flex_none()
            .items_center()
            .gap(px(6.))
            .h(px(24.))
            .px(px(CONTENT_INSET))
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .child(SharedString::from(if expanded {
                "▾ Graph (spike)"
            } else {
                "▸ Graph (spike)"
            }))
            .on_click(cx.listener(|this, _, _, cx| this.scm_toggle_graph(cx)))
            .into_any_element()
    }

    /// One interactive row. Deliberately an ordinary `div`: `Canvas::id`
    /// returns `None` and it implements no interactivity, so it registers no
    /// hitbox in prepaint — being drawn on top of these rows changes the
    /// painting order and nothing about where a click lands.
    fn graph_spike_row(&self, i: usize, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let sf = cx.theme().secondary;
        let id = spike_id(i);
        let selected = self.scm.graph.selected.as_deref() == Some(id.as_str());
        h_flex()
            .id(SharedString::from(format!("scm-graph-row-{i}")))
            .items_center()
            .h(px(GRAPH_ROW_H))
            .pl(px(GRAPH_PAD_L + GRAPH_LANE_W * 2. + 8.))
            .pr(px(CONTENT_INSET))
            .text_size(px(12.))
            .when(selected, |d| d.bg(cx.theme().accent.opacity(0.28)))
            .when(!selected, |d| d.hover(|s| s.bg(sf.opacity(0.9))))
            .cursor_pointer()
            .child(SharedString::from(text.to_string()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.scm.graph.selected = Some(spike_id(i));
                cx.notify();
            }))
            .into_any_element()
    }

    /// `right_panel_resize` rotated 90°: the same canvas-remembers-bounds plus
    /// `Rc<Cell>` pair, dragging the top edge of the history section instead of
    /// the left edge of the panel.
    fn graph_resize(&self, max: f32, cx: &mut Context<Self>) -> (AnyElement, AnyElement) {
        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let height = self.scm.graph.height.clone();
                let dragging = self.scm.graph.dragging.clone();
                move |_bounds, _state, window: &mut Window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let height = height.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window: &mut Window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            let bottom = b.origin.y + b.size.height;
                            let raw = (bottom - ev.position.y).as_f32();
                            height.set(raw.clamp(GRAPH_H_MIN, max));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window: &mut Window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();

        let active = self.scm.graph.dragging.get();
        let handle = div()
            .group("scm-graph-resize")
            .occlude()
            .absolute()
            .left_0()
            .top(px(-(GRAPH_HANDLE_H / 2.)))
            .w_full()
            .h(px(GRAPH_HANDLE_H))
            .flex()
            .items_center()
            .justify_center()
            .cursor_row_resize()
            .child(
                div()
                    .h(px(1.))
                    .w_full()
                    .when(active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("scm-graph-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.scm.graph.dragging.clone();
                move |_ev, window: &mut Window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element();

        (backing, handle)
    }
}

/// Stand-in for the sha the real rows will be keyed by.
fn spike_id(i: usize) -> String {
    format!("spike-{i}")
}
