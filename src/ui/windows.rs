//! The app-level window registry, and the single place that opens a window.
//!
//! tty7 used to have exactly one window, so `main` opened it inline and every
//! app-wide duty (tray, menus, the quit hook) could live in `Tty7App`'s
//! constructor. With several windows those duties have to belong to the *app*,
//! and anything that acts on "a window" — a tray click, `New Workspace`, the quit
//! hook walking every open workspace — needs a way to find them. That is this
//! module.
//!
//! The registry maps each live window to the [`WorkspaceId`] it displays.
//! Windows are transient views; workspaces are the persistent identity
//! (`core::session`). Exactly one window shows a given workspace at a time —
//! the daemon gives each pane a single subscriber, so two windows attached to
//! one workspace would have the second silently steal the first's output.
//! [`open`] enforces that by focusing an already-open workspace instead of
//! opening a second window onto it.

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, Styled as _, TitlebarOptions,
    WeakEntity, Window, WindowBounds, WindowOptions, point, px, size,
};
use gpui_component::{Root, TitleBar, WindowExt as _};

use crate::core::config::{Config, StartupMode};
use crate::core::session::{WorkspaceId, WorkspaceStore};
use crate::core::window_state::WindowState;
use crate::ui::app::Tty7App;

/// How far each additional window is offset from the one before it, so a new
/// window never lands exactly on top of an existing one (logical px).
const CASCADE_STEP: f32 = 28.0;

/// Default size for a window with nothing remembered.
const DEFAULT_SIZE: (f32, f32) = (1440.0, 900.0);

/// One live window and what it is showing.
struct WindowEntry {
    workspace: WorkspaceId,
    handle: AnyWindowHandle,
    /// Weak so a closed window's entity can drop normally; a dead handle is
    /// pruned on the next sweep rather than keeping the app alive.
    app: WeakEntity<Tty7App>,
}

/// Every window tty7 currently has open.
#[derive(Default)]
pub struct WindowRegistry {
    windows: Vec<WindowEntry>,
}

impl Global for WindowRegistry {}

impl WindowRegistry {
    /// Install the empty registry. Call once, before the first window opens.
    pub fn init(cx: &mut App) {
        cx.set_global(Self::default());
    }

    /// Number of live windows. Drives "is this the last window?" — the check
    /// that decides whether closing one quits the app.
    pub fn count(cx: &mut App) -> usize {
        Self::sweep(cx);
        cx.global::<Self>().windows.len()
    }

    /// The workspaces currently on screen, with the entity to read their tabs
    /// from. Used by the quit hook to record every window's final state.
    pub fn open_windows(cx: &mut App) -> Vec<(WorkspaceId, WeakEntity<Tty7App>)> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .map(|w| (w.workspace, w.app.clone()))
            .collect()
    }

    /// The window showing `workspace`, if one is open.
    pub fn window_for(cx: &mut App, workspace: WorkspaceId) -> Option<AnyWindowHandle> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .find(|w| w.workspace == workspace)
            .map(|w| w.handle)
    }

    /// The workspace of the most recently focused window — the sensible target
    /// for an app-wide action (a tray click, "open Settings") that needs *a*
    /// window but doesn't care which. Falls back to the first live window when
    /// the store has no opinion.
    pub fn most_recent(cx: &mut App) -> Option<WorkspaceId> {
        Self::sweep(cx);
        let active = WorkspaceStore::all(cx).active;
        let registry = cx.global::<Self>();
        active
            .filter(|id| registry.windows.iter().any(|w| w.workspace == *id))
            .or_else(|| registry.windows.first().map(|w| w.workspace))
    }

    /// The `Tty7App` showing `workspace`, if one is open.
    pub fn app_for(cx: &mut App, workspace: WorkspaceId) -> Option<WeakEntity<Tty7App>> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .find(|w| w.workspace == workspace)
            .map(|w| w.app.clone())
    }

    fn register(
        cx: &mut App,
        workspace: WorkspaceId,
        handle: AnyWindowHandle,
        app: WeakEntity<Tty7App>,
    ) {
        cx.global_mut::<Self>().windows.push(WindowEntry {
            workspace,
            handle,
            app,
        });
    }

    /// Forget a window. Idempotent — a window can be dropped by its own close
    /// path and then swept again when its entity finally releases.
    pub fn unregister(cx: &mut App, workspace: WorkspaceId) {
        cx.global_mut::<Self>()
            .windows
            .retain(|w| w.workspace != workspace);
    }

    /// Point an existing window at a different workspace, keeping its handle
    /// and entity. Used when the picker swaps a window's contents in place
    /// rather than opening a second window (see `Tty7App::switch_workspace`).
    pub fn rebind(cx: &mut App, from: WorkspaceId, to: WorkspaceId) {
        if let Some(entry) = cx
            .global_mut::<Self>()
            .windows
            .iter_mut()
            .find(|w| w.workspace == from)
        {
            entry.workspace = to;
        }
    }

    /// Drop entries whose `Tty7App` entity is gone. Windows can close through
    /// paths that never reach our own teardown (an OS-level close, a panic in a
    /// sibling view), so every read prunes first rather than trusting the list.
    fn sweep(cx: &mut App) {
        let dead: Vec<WorkspaceId> = cx
            .global::<Self>()
            .windows
            .iter()
            .filter(|w| w.app.upgrade().is_none())
            .map(|w| w.workspace)
            .collect();
        if dead.is_empty() {
            return;
        }
        cx.global_mut::<Self>()
            .windows
            .retain(|w| !dead.contains(&w.workspace));
    }
}

/// What a *brand-new* workspace's window starts with. Only consulted when the
/// window is opening on a freshly minted workspace — one restored from
/// `session.json` always rebuilds its saved tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FreshStart {
    /// A single default terminal, the way every previous launch of tty7 came
    /// up. What `New Workspace` and a genuine first run want: a window whose
    /// workspace has nothing in it yet is a window you asked for to work in.
    Shell,
    /// No tabs — the home page. Used at launch when there *are* saved
    /// workspaces but none were open at quit: the picker listing them is the
    /// whole point of that window, and a shell in front of it would bury it.
    HomePage,
}

/// Open a window on `workspace` — or on a brand-new workspace when `None`,
/// which starts with a single terminal (see [`open_with`] for the other case).
///
/// When that workspace already has a window, this focuses it instead of
/// opening a second one: two windows on one workspace would both attach the
/// same daemon panes, and the daemon's single-subscriber model means the
/// second attach silently kills the first window's terminal.
pub fn open(cx: &mut App, workspace: Option<WorkspaceId>) {
    open_with(cx, workspace, FreshStart::Shell);
}

/// [`open`], with a say in what a brand-new workspace comes up holding.
pub fn open_with(cx: &mut App, workspace: Option<WorkspaceId>, fresh: FreshStart) {
    if let Some(id) = workspace
        && let Some(handle) = WindowRegistry::window_for(cx, id)
    {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }

    let options = window_options(cx, workspace);
    // The registry needs the window's `Tty7App`, but `open_window` hands back
    // only the root view — so capture it on the way past.
    let mut created: Option<gpui::Entity<Tty7App>> = None;
    let opened = cx.open_window(options, |window, cx| {
        let app = cx.new(|cx| Tty7App::for_workspace(workspace, fresh, window, cx));
        created = Some(app.clone());
        // Root's own background is fully transparent: `Tty7App`'s root div is
        // the single owner of the window background (solid / gradient / image,
        // with the theme's alpha). A second paint here would compound the alpha
        // and read darker than the configured opacity.
        cx.new(|cx| Root::new(app, window, cx).bg(gpui::transparent_black()))
    });

    let handle = match opened {
        Ok(handle) => handle,
        Err(e) => {
            log::error!("failed to open window: {e}");
            return;
        }
    };
    let Some(app) = created else {
        log::error!("opened a window but its Tty7App was never built; not registering");
        return;
    };

    // Read back the workspace the window actually claimed — passing `None`
    // mints a fresh one, so the caller's id isn't authoritative.
    let id = app.read(cx).workspace;
    WindowRegistry::register(cx, id, handle.into(), app.downgrade());
    refresh_menu(cx);
}

/// Tell the user *once* that closing a window put its workspace away rather
/// than ending it, and where to find it again.
///
/// ⌘W is muscle memory and its result is off-screen, so the very first time it
/// detaches real work the user deserves a pointer — and never again after that.
/// Shown on whichever window survives; with none left (the app is quitting)
/// there is nowhere to put it and nothing to come back to yet, so it waits for
/// a later detach.
pub fn hint_detached(cx: &mut App, name: &str) {
    if cx.global::<Config>().workspace_detach_hint_seen {
        return;
    }
    let Some(target) = WindowRegistry::most_recent(cx) else {
        return;
    };
    let Some(handle) = WindowRegistry::window_for(cx, target) else {
        return;
    };
    cx.global_mut::<Config>().workspace_detach_hint_seen = true;
    cx.global::<Config>().save();
    // The title bar's workspace menu, not the macOS Window menu: Windows and
    // Linux have no menu bar, and the corner chip lists workspaces everywhere.
    let message =
        format!("“{name}” is still running — reopen it from the workspace menu in the title bar");
    let _ = handle.update(cx, |_, window, cx| {
        window.push_notification(message, cx);
    });
}

/// Rebuild the menu bar so the Window menu reflects the current workspace set.
///
/// macOS menus are static snapshots — nothing re-reads them when they open —
/// so every change to *which* workspaces exist has to push a new one. Called
/// on open / detach / switch / end, but deliberately not on ordinary tab edits:
/// a workspace's name comes from its repo and effectively never changes, so
/// rebuilding the whole menu bar per tab would be churn for nothing.
pub fn refresh_menu(cx: &mut App) {
    crate::ui::theme::set_menus(cx);
}

/// Most workspaces listed in the Window menu. Nine because that is how many
/// `SelectWorkspace1..9` actions exist — the same ceiling the tab shortcuts
/// use, and past which a flat menu stops being scannable anyway.
pub const MENU_SLOTS: usize = 9;

/// The Window menu's ordering, shared by the menu builder and the actions that
/// index into it so slot *n* always means the same workspace in both.
///
/// Open windows first (this is the macOS Window menu — its primary job is
/// listing what is on screen), then detached workspaces most-recent-first. That
/// second group is the whole point: a workspace closed with ⌘W has to be
/// visible *somewhere* or it may as well have been deleted.
pub fn menu_order(cx: &App) -> Vec<(WorkspaceId, bool)> {
    let all = WorkspaceStore::all(cx);
    let mut open: Vec<_> = all.workspaces.iter().filter(|w| w.open).collect();
    let mut closed: Vec<_> = all.workspaces.iter().filter(|w| !w.open).collect();
    open.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    closed.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    open.into_iter()
        .map(|w| (w.id, true))
        .chain(closed.into_iter().map(|w| (w.id, false)))
        .take(MENU_SLOTS)
        .collect()
}

/// How many of a workspace's panes are still running in the daemon. Zero means
/// closing it destroys nothing — every shell already exited — so the caller can
/// skip the confirmation prompt.
pub fn live_pane_count(cx: &App, workspace: WorkspaceId) -> usize {
    let Some(ws) = WorkspaceStore::all(cx).get(workspace) else {
        return 0;
    };
    let claimed = ws.pane_ids();
    if claimed.is_empty() {
        return 0;
    }
    // One short-lived control connection, only when there is something to ask
    // about — the picker renders far more often than a workspace is closed.
    let alive: std::collections::HashSet<u64> = crate::terminal::RemoteTerminal::list_panes()
        .into_iter()
        .filter(|p| p.alive)
        .map(|p| p.pane_id)
        .collect();
    claimed.iter().filter(|id| alive.contains(id)).count()
}

/// Confirm, then stop `workspace`. Skips the prompt when nothing is running —
/// there is nothing to lose and it would be pure friction.
pub fn confirm_and_stop(cx: &mut App, window: &mut Window, workspace: WorkspaceId) {
    confirm_destructive(cx, window, workspace, "Stop", stop_workspace);
}

/// Confirm, then delete `workspace`. Always asks: even with every shell
/// already exited, the saved layout is still something to lose.
pub fn confirm_and_delete(cx: &mut App, window: &mut Window, workspace: WorkspaceId) {
    confirm_destructive(cx, window, workspace, "Delete", delete_workspace);
}

/// Shared confirm-then-act path for the two destructive workspace actions.
///
/// A free function rather than a `Tty7App` method because the title-bar menu's
/// row buttons run inside a menu builder, which has a `Window` and an `App` but
/// no entity to call a method on.
fn confirm_destructive(
    cx: &mut App,
    window: &mut Window,
    workspace: WorkspaceId,
    verb: &'static str,
    act: fn(&mut App, WorkspaceId),
) {
    let live = live_pane_count(cx, workspace);
    let name = WorkspaceStore::all(cx)
        .get(workspace)
        .map(|w| w.display_name())
        .unwrap_or_else(|| "this workspace".to_string());
    if live == 0 && verb == "Stop" {
        act(cx, workspace);
        return;
    }
    let detail = match (live, verb) {
        (0, _) => "Its layout and working directories will be forgotten.".to_string(),
        (1, "Delete") => "1 running session will be ended and its layout forgotten.".to_string(),
        (n, "Delete") => format!("{n} running sessions will be ended and the layout forgotten."),
        (1, _) => "1 running session will be ended.".to_string(),
        (n, _) => format!("{n} running sessions will be ended."),
    };
    // Title Case, like every other prompt title in the app — this one used to
    // lowercase "workspace" while its siblings read "Close Window?" /
    // "Quit and Stop Daemon?".
    let answer = window.prompt(
        gpui::PromptLevel::Warning,
        &format!("{verb} Workspace \u{201c}{name}\u{201d}?"),
        Some(&detail),
        &["Cancel", verb],
        cx,
    );
    cx.spawn(async move |cx| {
        // Index 1 == the verb button; Cancel and a dismissed prompt both leave
        // the workspace alone.
        if let Ok(1) = answer.await {
            cx.update(|cx| act(cx, workspace));
        }
    })
    .detach();
}

/// Stop a workspace: kill every pane it owns in the daemon, and close the
/// window showing it.
///
/// The workspace *record* survives — its tabs, split layout and each pane's cwd
/// stay on file — so reopening it later rebuilds the same arrangement with
/// fresh shells. That is the difference from [`delete_workspace`], which throws
/// the record away too.
///
/// Callers confirm first when [`live_pane_count`] is non-zero; with nothing
/// running there is nothing to lose.
pub fn stop_workspace(cx: &mut App, workspace: WorkspaceId) {
    if let Some(ws) = WorkspaceStore::all(cx).get(workspace) {
        for pane_id in ws.pane_ids() {
            crate::terminal::RemoteTerminal::kill_pane(pane_id);
        }
    }
    // One workspace is shown by exactly one window, so stopping the work means
    // the window goes with it — leaving an empty frame behind reads as a
    // half-finished action.
    close_window_for(cx, workspace);
    WorkspaceStore::close_window(cx, workspace);
    refresh_menu(cx);
}

/// Delete a workspace outright: stop it, then forget it entirely. Irreversible
/// — nothing about the layout survives.
pub fn delete_workspace(cx: &mut App, workspace: WorkspaceId) {
    stop_workspace(cx, workspace);
    WorkspaceStore::remove(cx, workspace);
    refresh_menu(cx);
}

/// Close whichever window is showing `workspace`, if any.
///
/// The last window is the exception: it stays, swapped onto a fresh blank
/// workspace, because a windowless tty7 left in the Dock stops responding to
/// clicks (#147).
fn close_window_for(cx: &mut App, workspace: WorkspaceId) {
    let showing = WindowRegistry::app_for(cx, workspace);
    let Some(handle) = WindowRegistry::window_for(cx, workspace) else {
        return;
    };
    let Some(app) = showing.and_then(|weak| weak.upgrade()) else {
        return;
    };

    if WindowRegistry::count(cx) > 1 {
        WindowRegistry::unregister(cx, workspace);
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        return;
    }

    let (fresh, session) = WorkspaceStore::claim(cx, None);
    WindowRegistry::rebind(cx, workspace, fresh);
    let _ = handle.update(cx, |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(fresh, session, window, cx)
        });
    });
}

/// Where a new window should appear: the workspace's own remembered geometry
/// first (that is where the user left *this* workspace), then the shared
/// `window.json` fallback, then a centred default — each cascaded so it does
/// not land exactly on an existing window.
fn window_options(cx: &mut App, workspace: Option<WorkspaceId>) -> WindowOptions {
    let remember = cx.global::<Config>().remember_window_size;
    let remembered = remember
        .then(|| {
            workspace
                .and_then(|id| WorkspaceStore::all(cx).get(id).and_then(|w| w.window))
                .or_else(WindowState::load)
        })
        .flatten();

    let existing = WindowRegistry::count(cx);
    let bounds = match remembered {
        // A remembered window that no longer touches any display (monitor
        // unplugged, resolution change) keeps its size but re-centers.
        Some(state) => {
            let bounds = state.bounds();
            if cx.displays().iter().any(|d| d.bounds().intersects(&bounds)) {
                bounds
            } else {
                Bounds::centered(None, bounds.size, cx)
            }
        }
        None => Bounds::centered(None, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx),
    };
    let bounds = cascade(bounds, existing);

    // Launch state from config: a normal window, or maximized / fullscreen.
    // Each variant still carries the bounds above as the size to restore to
    // when the user un-maximizes / exits fullscreen. Only the *first* window
    // honors maximized/fullscreen — a second window forced fullscreen would
    // hide the one the user was just in.
    let window_bounds = match cx.global::<Config>().startup_mode {
        _ if existing > 0 => WindowBounds::Windowed(bounds),
        StartupMode::Normal => WindowBounds::Windowed(bounds),
        StartupMode::Maximized => WindowBounds::Maximized(bounds),
        StartupMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    };

    WindowOptions {
        window_bounds: Some(window_bounds),
        // Start from the component defaults but nudge the traffic lights down
        // so they stay vertically centred in our taller (40px) title bar — see
        // `TitleBar::new().h(..)` in `app.rs`. `apply_theme` re-pins the same
        // position after appearance changes.
        titlebar: Some(TitlebarOptions {
            traffic_light_position: Some(crate::ui::theme::traffic_light_position()),
            ..TitleBar::title_bar_options()
        }),
        // Non-opaque from creation: macOS 26 ignores a runtime flip to
        // transparent, so the opacity slider only works on a window born this
        // way (see `theme::background_appearance`).
        window_background: crate::ui::theme::background_appearance(cx),
        ..Default::default()
    }
}

/// Offset `bounds` by one cascade step per existing window, so opening several
/// windows in a row doesn't stack them invisibly on top of each other.
fn cascade(bounds: Bounds<gpui::Pixels>, existing: usize) -> Bounds<gpui::Pixels> {
    if existing == 0 {
        return bounds;
    }
    // Wrap after a few steps so a long-lived session doesn't march windows off
    // the bottom-right of the display.
    let step = (existing % 5) as f32 * CASCADE_STEP;
    Bounds {
        origin: bounds.origin + point(px(step), px(step)),
        size: bounds.size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds_at(x: f32, y: f32) -> Bounds<gpui::Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(800.), px(600.)),
        }
    }

    #[test]
    fn the_first_window_is_not_cascaded() {
        let b = bounds_at(100., 100.);
        assert_eq!(cascade(b, 0).origin, b.origin);
    }

    #[test]
    fn each_extra_window_steps_down_and_right() {
        let b = bounds_at(100., 100.);
        assert_eq!(
            cascade(b, 1).origin,
            point(px(100. + CASCADE_STEP), px(100. + CASCADE_STEP))
        );
        assert_eq!(
            cascade(b, 2).origin,
            point(px(100. + 2. * CASCADE_STEP), px(100. + 2. * CASCADE_STEP))
        );
        // Size is never touched — only the origin moves.
        assert_eq!(cascade(b, 3).size, b.size);
    }

    #[test]
    fn cascade_wraps_so_windows_never_march_off_screen() {
        let b = bounds_at(100., 100.);
        // The 5th extra window is back at the un-offset origin rather than
        // 5 steps further down-right.
        assert_eq!(cascade(b, 5).origin, b.origin);
        assert_eq!(cascade(b, 6).origin, cascade(b, 1).origin);
    }
}
