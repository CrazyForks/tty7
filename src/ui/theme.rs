//! The menu bar and theme application — the window "chrome" that sits outside
//! the tab/pane shell. `set_menus` (re)builds the macOS menu; `apply_theme`
//! paints gpui-component's `Theme` from the active color theme (see
//! `ui::presets`) and publishes the terminal-facing palette.

use gpui::{
    App, Background, Hsla, Menu, MenuItem, OsAction, Pixels, Point, SystemMenuType, Window,
    WindowBackgroundAppearance, linear_color_stop, linear_gradient, point, px, rgb,
};
use gpui_component::{Theme, ThemeMode};

use crate::core::actions::*;
use crate::core::config::Config;
use crate::terminal::view::{
    ClearScrollback, CopyText, CutText, FindInTerminal, FindNext, FindPrevious, PasteText,
    RedoEdit, SelectAll, UndoEdit,
};
use crate::ui::presets;
use crate::ui::presets::Fill;

/// The traffic-light origin, nudged down from the macOS default so the buttons
/// stay vertically centred in our taller (40px) title bar. Shared between the
/// window's initial `TitlebarOptions` (see `main.rs`) and `apply_theme`, which
/// re-pins it after each theme change — macOS resets the buttons to their
/// default (higher) position when the app appearance changes, and gpui only
/// repositions them on the next resize/activation, so they'd briefly sit too
/// high until then.
pub(crate) fn traffic_light_position() -> Point<Pixels> {
    point(px(9.), px(13.))
}

/// (Re)build the macOS menu bar.
///
/// Menu order and contents follow the macOS HIG's standard set — App, File,
/// Edit, View, Window, Help — because that is where a Mac user's hand goes
/// before they read a single label. The app used to ship four menus in the
/// order App / Shell / Window / View with no Edit at all, which put Copy and
/// Paste nowhere but a right-click and made the whole bar read as improvised.
///
/// Two deliberate departures from a stock bar:
///
/// * There is no "Shell" menu. Its contents (new/close/split/rename) are File's
///   job everywhere else, and the name collided with Settings → Shell, which
///   configures something entirely different — the program a pane launches.
/// * "Restart Daemon…" lives at the bottom of Help, not near Settings. It is a
///   break-glass repair, it ends every running shell, and it has no business
///   one slot away from ⌘,.
pub(crate) fn set_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("tty7").items([
            MenuItem::action("About tty7", About),
            MenuItem::action("Check for Updates…", CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide tty7", HideApp),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit tty7", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Tab", NewTab),
            MenuItem::action("New Workspace", NewWorkspace),
            MenuItem::action("New Worktree Tab", NewWorktreeTab),
            MenuItem::separator(),
            MenuItem::action("Split Right", SplitRight),
            MenuItem::action("Split Down", SplitDown),
            MenuItem::separator(),
            MenuItem::action("Rename Tab…", RenameTab),
            MenuItem::action("Copy Working Directory", CopyWorkingDirectory),
            MenuItem::separator(),
            MenuItem::action("Close Pane / Tab", CloseActiveTab),
            MenuItem::action("Close Other Tabs", CloseOtherTabs),
            MenuItem::action("Close Tabs to the Right", CloseTabsToTheRight),
            MenuItem::action("Reopen Closed Tab", ReopenClosedTab),
            MenuItem::separator(),
            MenuItem::action("Rename Workspace…", RenameWorkspace),
            // Separated: the only item above the rule that touches running
            // sessions is none of them — closing a window or a tab leaves the
            // shells alive in the daemon. Stop ends them but keeps the layout.
            MenuItem::action("Stop Workspace…", StopWorkspace),
            // Alone at the very bottom, behind its own rule: the one
            // irreversible item in the entire menu bar. It used to sit directly
            // under Stop, distinguishable only by the verb.
            MenuItem::separator(),
            MenuItem::action("Delete Workspace…", DeleteWorkspace),
        ]),
        // `os_action` routes these through the standard Cut/Copy/Paste/Select All
        // selectors, so they behave like every other Mac app's Edit menu (and stay
        // enabled via the app delegate) while still dispatching our own actions.
        // They carry no key-equivalent glyph: the chords are handled inline in
        // `terminal::view::handle_cmd_shortcut` rather than as registered
        // bindings, because ⌃C has to fall through to SIGINT when nothing is
        // selected — a registered binding would swallow it.
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", UndoEdit, OsAction::Undo),
            MenuItem::os_action("Redo", RedoEdit, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", CutText, OsAction::Cut),
            MenuItem::os_action("Copy", CopyText, OsAction::Copy),
            MenuItem::os_action("Paste", PasteText, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find…", FindInTerminal),
            MenuItem::action("Find Next", FindNext),
            MenuItem::action("Find Previous", FindPrevious),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", TogglePalette),
            MenuItem::separator(),
            MenuItem::action("Increase Font Size", IncreaseFontSize),
            MenuItem::action("Decrease Font Size", DecreaseFontSize),
            MenuItem::action("Reset Font Size", ResetFontSize),
            MenuItem::separator(),
            // The three docks and the tab rail's placement — the most literally
            // "view" things in the app, and until now reachable only by chord.
            MenuItem::action("Left Sidebar", ToggleLeftPanel),
            MenuItem::action("Right Panel", ToggleRightPanel),
            MenuItem::action("Code Panel", ToggleCodePanel),
            MenuItem::action("Tab Bar Position", ToggleTabSidebar),
            MenuItem::separator(),
            MenuItem::action("Focus Next Pane", FocusNextPane),
            MenuItem::action("Focus Previous Pane", FocusPrevPane),
            MenuItem::action("Zoom Pane", ToggleMaximizePane),
            MenuItem::separator(),
            MenuItem::action("Clear Scrollback", ClearScrollback),
            MenuItem::separator(),
            MenuItem::action("Enter Full Screen", ToggleFullscreen),
        ]),
        Menu::new("Window").items(window_menu_items(cx)),
        Menu::new("Help").items([
            MenuItem::action("tty7 Documentation", OpenDocumentation),
            MenuItem::action("Keyboard Shortcuts", ShowKeyboardShortcuts),
            MenuItem::separator(),
            MenuItem::action("Join the Discord", OpenDiscord),
            MenuItem::action("Report an Issue…", ReportIssue),
            MenuItem::separator(),
            // Force a fresh background daemon (so a newly granted macOS permission
            // such as Full Disk Access takes effect). The trailing "…" signals the
            // confirmation prompt; it ends every running session.
            MenuItem::action("Restart Daemon…", RestartDaemon),
        ]),
    ]);
}

/// The Window menu's contents: every workspace tty7 knows about, on screen or
/// not.
///
/// This is what makes ⌘W honest. Closing a window only *detaches* its
/// workspace — the shells keep running in the daemon — but a detached
/// workspace the user can't see may as well have been deleted. The Window menu
/// is where a Mac user already looks for "what do I have open", so putting the
/// detached ones right below the open ones costs no learning at all.
///
/// Slot order comes from [`crate::ui::windows::menu_order`], shared with the
/// `SelectWorkspace1..9` handlers so slot *n* means the same thing in both.
fn window_menu_items(cx: &App) -> Vec<MenuItem> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let order = crate::ui::windows::menu_order(cx);
    let store = crate::core::session::WorkspaceStore::all(cx);

    // The same slot→action mapping the title-bar chip's menu uses, so slot *n*
    // dispatches identically wherever it was clicked.
    let slot_action = crate::ui::tab_strip::select_workspace_action;

    // Minimize / Zoom first: every Mac app's Window menu opens with them, and a
    // menu that jumps straight into a bespoke list reads as if the standard ones
    // were forgotten. The workspace roster follows behind a rule.
    let mut items = vec![
        MenuItem::action("Minimize", MinimizeWindow),
        MenuItem::action("Zoom", ZoomWindow),
        MenuItem::separator(),
    ];
    let workspace_start = items.len();
    let mut separated = false;
    for (i, (id, open)) in order.iter().enumerate() {
        let Some(workspace) = store.get(*id) else {
            continue;
        };
        let Some(action) = slot_action(i) else { break };
        // One rule between the two groups: what's on screen, then what's put
        // away. Only drawn once, and never as a leading rule.
        if !open && !separated {
            separated = true;
            // Compared against the roster's own start, not the whole menu: with
            // Minimize/Zoom above, `items` is never empty and the old check
            // would have drawn a second rule directly under the first.
            if items.len() > workspace_start {
                items.push(MenuItem::Separator);
            }
        }
        let label = if *open {
            workspace.display_name()
        } else {
            // The age is the useful discriminator among detached ones — several
            // may share a repo name.
            format!(
                "{}  —  {}",
                workspace.display_name(),
                crate::ui::home::relative_time(now, workspace.last_active)
            )
        };
        items.push(MenuItem::Action {
            name: label.into(),
            action,
            os_action: None,
            checked: false,
            disabled: false,
        });
    }
    if items.len() == workspace_start {
        // Never leave the roster empty — a Window menu that lists no windows
        // reads as broken. The one workspace that must exist is the current one.
        items.push(MenuItem::action("New Workspace", NewWorkspace));
    }
    items
}

/// The actual window-background paint for the active theme: a flat color or a
/// real two-stop linear gradient (vertical = CSS `to bottom`, horizontal =
/// `to right`), with the theme's window opacity carried in the stops' alpha so
/// a translucent theme shows through gradients exactly like solids.
pub(crate) fn window_background(bg: &presets::ActiveBackground) -> Background {
    let alpha = bg.opacity.unwrap_or(1.0);
    let stop = |c: u32| -> Hsla {
        let mut h: Hsla = rgb(c).into();
        h.a = alpha;
        h
    };
    match bg.fill {
        Fill::Solid(c) => stop(c).into(),
        Fill::Vertical { top, bottom } => linear_gradient(
            180.,
            linear_color_stop(stop(top), 0.),
            linear_color_stop(stop(bottom), 1.),
        ),
        Fill::Horizontal { left, right } => linear_gradient(
            90.,
            linear_color_stop(stop(left), 0.),
            linear_color_stop(stop(right), 1.),
        ),
    }
}

/// Whether the OS is currently in dark mode, as gpui reports it. Only
/// meaningful while the native appearance isn't pinned (see
/// [`sync_native_appearance`]) — a pinned appearance reports the pin, not the
/// OS setting, which is why callers gate on `Config::theme_follow_system`.
pub(crate) fn system_dark(cx: &App) -> bool {
    matches!(
        cx.window_appearance(),
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    )
}

/// The id of the theme that should be on screen right now: the light/dark
/// slot matching the OS appearance while `Config::theme_follow_system` is on,
/// otherwise the manual `Config::theme_preset`.
pub(crate) fn effective_preset_id(cx: &App) -> String {
    let config = cx.global::<Config>();
    if !config.theme_follow_system {
        config.theme_preset.clone()
    } else if system_dark(cx) {
        config.theme_preset_dark.clone()
    } else {
        config.theme_preset_light.clone()
    }
}

/// The background appearance the window should be *created* with: Blurred when
/// the effective theme wants blur, otherwise Transparent — never Opaque, so the
/// opacity slider works live (see the comment in [`apply_theme`]).
pub(crate) fn background_appearance(cx: &App) -> WindowBackgroundAppearance {
    let config = cx.global::<Config>();
    let theme = presets::by_id(cx, &effective_preset_id(cx));
    if config.window_blur.unwrap_or(theme.blur) {
        WindowBackgroundAppearance::Blurred
    } else {
        WindowBackgroundAppearance::Transparent
    }
}

/// Paint gpui-component's `Theme` from the active color theme (resolved by
/// [`effective_preset_id`]). The theme's inferred `dark` brightness picks the
/// component `ThemeMode`; every shell surface is then derived from the theme's
/// background/foreground (see `Theme::neutrals`). Also publishes the
/// terminal-facing palette as the `ActivePalette` global so the renderer matches,
/// and applies the theme's window opacity/blur.
pub(crate) fn apply_theme(mut window: Option<&mut Window>, cx: &mut App) {
    let follow = cx.global::<Config>().theme_follow_system;
    // While following the OS the native pin must be released *before* the
    // theme is resolved: `effective_preset_id` reads the system appearance
    // through `effectiveAppearance`, which keeps reporting the pinned value
    // until the pin is cleared.
    if follow {
        sync_native_appearance(None);
    }
    let theme = presets::by_id(cx, &effective_preset_id(cx));
    let config = cx.global::<Config>();
    let mode = if theme.dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    // Window opacity / blur: the global config override wins when set (so a
    // chosen translucency survives theme switches); otherwise the theme's own
    // values apply. Only an opacity below 1.0 makes the window translucent.
    let opacity = config.window_opacity.or(theme.opacity).filter(|o| *o < 1.0);
    let blur = config.window_blur.unwrap_or(theme.blur);
    // Force the native macOS chrome (traffic lights, system menus, scrollbars)
    // into the theme's own light/dark mode regardless of the OS setting —
    // only while *not* following the OS, where the chrome should track the
    // system (and pinning would blind `system_dark` to OS flips).
    if !follow {
        sync_native_appearance(Some(theme.dark));
    }
    let m = theme.neutrals();
    let active = theme.active_palette();

    // Never `Opaque`: on macOS 26 (Tahoe) flipping a window's opacity after
    // creation doesn't reach the compositor — the window keeps compositing
    // against black (verified empirically; the framebuffer alpha was correct
    // but a red window behind never bled through). So the window is created
    // non-opaque (see `background_appearance`, used by `main.rs`) and stays
    // that way; a fully opaque theme simply paints alpha-1.0 content, which is
    // visually identical. Only the Transparent↔Blurred flip happens here at
    // runtime (it adds/removes an NSVisualEffectView, which does work live).
    if let Some(window) = window.as_deref_mut() {
        let bg_appearance = if blur {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Transparent
        };
        window.set_background_appearance(bg_appearance);
    }

    Theme::change(mode, window.as_deref_mut(), cx);
    // Publish the terminal palette before borrowing the theme mutably.
    cx.set_global(active);
    // Publish the render-facing background (fill/opacity/image) for the root
    // view, which paints gradients and the background image for real —
    // gpui-component's `Theme.background` below only carries the representative
    // solid.
    cx.set_global(presets::ActiveBackground {
        fill: theme.background.clone(),
        opacity,
        image: theme.image.clone(),
    });

    let t = Theme::global_mut(cx);
    // The window base carries the theme's opacity so a translucent/blurred theme
    // actually shows through; opaque themes (opacity None) stay fully opaque.
    let mut base: Hsla = rgb(m.background).into();
    if let Some(o) = opacity {
        base.a = o;
    }
    t.background = base; // terminal / window base
    t.foreground = rgb(m.foreground).into(); // default text
    t.border = rgb(m.border).into();
    t.secondary = rgb(m.secondary).into(); // hover chips (+ / tab)
    t.muted = rgb(m.muted).into();
    t.muted_foreground = rgb(m.muted_foreground).into(); // inactive tab text
    t.popover = rgb(m.popover).into(); // elevated surfaces
    // gpui-component paints popovers/menus (context menu, dropdowns) from
    // `tokens.popover` / `tokens.popover_foreground`, NOT the `popover*` fields —
    // so the menu background ignored our theme and fell back to the stock surface
    // (looking off-theme). Mirror the theme onto the tokens, same gotcha as the
    // sidebar below.
    t.tokens.popover = Hsla::from(rgb(m.popover)).into();
    t.tokens.popover_foreground = Hsla::from(rgb(m.foreground)).into();

    // Context menus and dropdowns highlight the hovered/selected row from
    // `tokens.accent` (fill) + `accent_foreground` (text) — see gpui-component's
    // `MenuItemElement`. Left unset, that highlight falls back to the stock
    // saturated accent, which snaps hard against this app's soft mix-based
    // palette (the "生硬" hover). Point it at the same soft fill the command
    // palette uses for its selected row (`list_active`, mix 0.17) so context
    // menu, dropdown and palette share one hover language; keep the text at
    // `foreground` so it stays legible on the low-contrast fill instead of the
    // stock inverted accent text. The plain `accent`/`accent_foreground` fields
    // feed the same highlight in the input completion / code-action popovers, so
    // mirror both the fields and the tokens to keep every menu surface in step.
    let accent_fill = rgb(m.list_active);
    let accent_text: Hsla = rgb(m.foreground).into();
    t.accent = accent_fill.into();
    t.accent_foreground = accent_text;
    t.tokens.accent = Hsla::from(accent_fill).into();
    t.tokens.accent_foreground = accent_text.into();

    // Primary buttons (Connect, Reconnect, Trust, Save…) fill from
    // `tokens.button_primary`; the raw default is the foreground — a pure
    // near-black in a light theme, which reads harsh. Nudge it toward the
    // background so it lands on a softer dark charcoal (and, symmetrically, a
    // slightly dimmed near-white in dark themes). We set the whole primary token
    // family (both the plain `primary*` fields — used for the border and outline
    // text — and the `button_primary*` tokens the fill actually reads) so every
    // primary button shifts together, hover/pressed included. The stock
    // `button_primary_foreground` stays legible on top either way.
    let primary_base: Hsla = rgb(presets::mix(m.foreground, m.background, 0.20)).into();
    let primary_hover: Hsla = rgb(presets::mix(m.foreground, m.background, 0.30)).into();
    let primary_active: Hsla = rgb(presets::mix(m.foreground, m.background, 0.10)).into();
    t.primary = primary_base;
    t.primary_hover = primary_hover;
    t.primary_active = primary_active;
    t.tokens.primary = primary_base.into();
    t.tokens.primary_hover = primary_hover.into();
    t.tokens.primary_active = primary_active.into();
    t.tokens.button_primary = primary_base.into();
    t.tokens.button_primary_hover = primary_hover.into();
    t.tokens.button_primary_active = primary_active.into();

    t.caret = rgb(m.caret).into();
    t.selection = rgb(m.selection).into(); // text selection highlight

    // Round every gpui-component widget (buttons, inputs, selects, switches,
    // segmented controls, menus) to match the shell's own hand-rolled chrome,
    // which uses `rounded_lg` (8px) for tab chips, title-bar tiles and the
    // settings steppers. gpui-component defaults to 6px, so stock controls read a
    // hair boxier than everything around them; pinning `radius` to 8 makes the
    // widgets and the chrome share one corner language instead of two. The
    // hand-rolled chrome sets explicit radii, so it's unaffected — this only
    // pulls the stock widgets into line.
    t.radius = px(8.);

    // Settings sidebar. NOTE: gpui-component's Sidebar paints its column from
    // `tokens.sidebar` (and the active chip from `tokens.sidebar_accent`), NOT
    // the `sidebar*` color fields — so those must be set on `tokens` or the
    // override is a no-op and the column falls back to the stock surface.
    let sidebar_bg = rgb(m.sidebar);
    let sidebar_sel = rgb(m.sidebar_sel);
    t.sidebar = sidebar_bg.into();
    t.tokens.sidebar = Hsla::from(sidebar_bg).into();
    t.sidebar_border = rgb(m.border).into();
    t.sidebar_foreground = rgb(m.sidebar_fg).into();
    t.sidebar_accent = sidebar_sel.into();
    t.tokens.sidebar_accent = Hsla::from(sidebar_sel).into();
    t.sidebar_accent_foreground = rgb(m.foreground).into();

    // Flatten gpui-component's list selection highlight (used by the command
    // palette) into a single soft fill — no blue ring, no accent tint — so it
    // matches this app's minimal aesthetic instead of the stock look. Keep
    // `active_highlight` on (the alternative path tints with the shared
    // `accent`), but make the ring colour equal the fill so the box disappears.
    t.list.active_highlight = true;
    t.list_active = rgb(m.list_active).into();
    t.list_active_border = rgb(m.list_active).into();
    t.list_hover = rgb(m.list_hover).into();

    // `sync_native_appearance` above may have flipped the macOS app appearance,
    // which resets the traffic-light buttons to their default (higher) position.
    // gpui doesn't reposition them on an appearance change (only on
    // resize/activation/title changes), so re-pin our centred position now —
    // otherwise the buttons briefly sit too high until the next such event. Same
    // immediate-re-move pattern gpui itself uses after `setRepresentedFilename`.
    #[cfg(target_os = "macos")]
    if let Some(window) = window.as_deref_mut() {
        window.set_traffic_light_position(traffic_light_position());
    }
}

/// Apply `Config::mouse_hide_while_typing` to GPUI's cursor-hide policy: hide the
/// pointer while typing when on, never when off. Called at startup and whenever
/// the config changes (setter + hot-reload) so the switch takes effect live.
pub(crate) fn apply_cursor_hide_mode(cx: &mut App) {
    let mode = if cx.global::<Config>().mouse_hide_while_typing {
        gpui::CursorHideMode::OnTypingAndAction
    } else {
        gpui::CursorHideMode::Never
    };
    cx.set_cursor_hide_mode(mode);
}

/// Pin the macOS app appearance to the active theme's light/dark mode
/// (`Some(dark)`), or release the pin so it follows the OS `Appearance`
/// setting again (`None`, used while `Config::theme_follow_system` is on).
///
/// macOS draws the native traffic-light buttons according to the window's
/// effective appearance. With a dark tty7 theme on a light-mode macOS, the
/// system paints the *light-style* inactive (unfocused) traffic lights — heavy
/// mid-grey circles that look filthy on the dark titlebar. gpui only ever
/// *reads* `effectiveAppearance` (`WindowAppearance::from_native`); it exposes
/// no setter, so we pin `NSApplication.appearance` ourselves via AppKit. This
/// also keeps system menus, context menus and scrollbars in the right mode.
#[cfg(target_os = "macos")]
fn sync_native_appearance(dark: Option<bool>) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    };

    // `apply_theme` is always invoked on the gpui app (main) thread; bail
    // defensively rather than panic if that ever stops holding.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let appearance = dark.and_then(|dark| {
        // SAFETY: reading the framework-provided appearance-name statics.
        let name = unsafe {
            if dark {
                NSAppearanceNameDarkAqua
            } else {
                NSAppearanceNameAqua
            }
        };
        NSAppearance::appearanceNamed(name)
    });
    // `None` here means "inherit from the system" — the AppKit way to unpin.
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}

#[cfg(not(target_os = "macos"))]
fn sync_native_appearance(_dark: Option<bool>) {}
