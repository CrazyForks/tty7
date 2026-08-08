//! The source control panel body: four groups of file rows over the working
//! tree status, in the order git itself talks about them.
//!
//! The panel is rendered from `WorkingTreeStatus`, which knows the difference
//! between the index and the working tree. That is the whole reason the old
//! flat list had to go: it ran one `git diff HEAD`, so it could not tell a
//! staged change from an unstaged one and showed every row the letter `M`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, Focusable as _, SharedString, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};

use tty7_core::core::git::diff::MAX_RENDERED_FILES;
use tty7_core::core::git::ops::GitOp;
use tty7_core::core::git::status::{
    ChangeCode, DecoStatus, RepoPath, StatusEntry, WorkingTreeStatus,
};

use crate::terminal::git_data::status_of;
use crate::terminal::git_diff::DiffSource;
use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::host_ops::{HostId, SharedHost};
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::right_panel::git_badge;
use crate::ui::scm::ScmIntent;
use crate::ui::scm::path::split_display_path;
use crate::ui::scm::state::{RepoKey, ScmGroup};
use crate::ui::scm::status::{status_color, status_glyph};

/// A file row, and the group header above it. Both 24px, so the list reads as
/// one grid rather than as headers with a list hanging off them.
const ROW_H: f32 = 24.;

/// The status letter's column, from `git_badge`. The group chevron sits in a
/// box of exactly this width so the two line up in one column down the panel.
const BADGE_W: f32 = 14.;

/// Rows are laid out inside this inset and then pad themselves back out, so a
/// hovered row's background is wider than its text on both sides.
const ROW_INSET: f32 = 4.;

/// The row-button tile, one step below `TILE_SIZE_SM`.
///
/// These belong next to the other tile sizes in `app.rs`; they are here
/// because that file is being rewritten elsewhere this cycle, and moving them
/// is a one-line change once it settles.
pub(crate) const TILE_SIZE_XS: f32 = 18.;
pub(crate) const TILE_GLYPH_XS: f32 = 11.;

/// The key context the message box installs, and the one `ScmCommit` is
/// bound inside. The two are the same string on purpose: a binding whose
/// context nothing attaches is a binding that never fires.
pub(crate) const COMMIT_KEY_CONTEXT: &str = "ScmCommit";

/// Untracked files past this many start folded. A fresh clone of a repository
/// with a stale `.gitignore` can put thousands of them in front of the three
/// changes the user came to look at.
const UNTRACKED_AUTO_COLLAPSE: usize = 20;

/// How long to wait before asking git again about a directory that answered
/// with nothing.
///
/// `scm_refresh` is safe to call every frame — it de-duplicates in-flight
/// probes and skips fresh ones. What it cannot do is notice that a probe came
/// back empty: a repository we never got a status for stays stale forever, so
/// without this the panel would start a new `git status` on every frame.
const PROBE_RETRY: Duration = Duration::from_secs(2);

/// What the panel knows about the directory the active pane is sitting in.
enum RepoLookup {
    /// Nothing has answered yet — the tab's own probe is still out.
    Pending,
    NotARepo,
    Root(PathBuf),
}

impl Tty7App {
    pub(crate) fn render_panel_scm(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.scm_watch_status(cx);

        let Some((host, cwd)) = self.scm_pane_target(window, cx) else {
            let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
            let body = self.panel_empty(
                t(L10nKey::PanelNoWorkingDirectory),
                Some(t(L10nKey::PanelNoWorkingDirectoryHint)),
                cx,
            );
            return self.scm_shell(title, body);
        };

        let root = match self.scm_repo_root(&host, &cwd, cx) {
            RepoLookup::Pending => {
                let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
                let body = self.panel_empty(t(L10nKey::PanelLoading), None, cx);
                return self.scm_shell(title, body);
            }
            RepoLookup::NotARepo => {
                let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
                let body = self.panel_empty(
                    t(L10nKey::PanelNotAGitRepo),
                    Some(t(L10nKey::PanelNotAGitRepoHint)),
                    cx,
                );
                return self.scm_shell(title, body);
            }
            RepoLookup::Root(root) => root,
        };

        self.scm_probe(&host, &root, cx);
        let Some(status) = self.scm_seen_status(host.id(), &root, cx) else {
            let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
            let body = self.panel_empty(t(L10nKey::PanelLoading), None, cx);
            return self.scm_shell(title, body);
        };

        let repo = RepoKey {
            host: host.id(),
            root,
        };
        self.scm.repo = Some(repo.clone());

        let count = (status.total_entries > 0).then(|| status.total_entries.to_string());
        let title = self.panel_title(t(L10nKey::PanelScmTitle), count, None, window, cx);

        let commit = self.scm_commit_box(&repo, &status, window, cx);
        let buttons = self.scm_commit_buttons(&repo, &status, cx);
        let body = if status.is_clean() {
            self.panel_empty(
                t(L10nKey::PanelNoChanges),
                Some(t(L10nKey::PanelNoChangesHint)),
                cx,
            )
        } else {
            self.scm_groups(&repo, &status, cx)
        };
        self.scm_shell_with(title, vec![commit, buttons], body)
    }

    /// The message box.
    ///
    /// `key_context` rather than a focus trap: `secondary-enter` is
    /// `ToggleFullscreen` at the window level, and the two coexist because
    /// gpui resolves a keystroke by walking outwards from the focused node —
    /// the narrow context wins while the box has focus, and the window keeps
    /// the chord everywhere else.
    fn scm_commit_box(
        &mut self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = self.scm_commit_input(repo, status, window, cx);
        let focused = input.read(cx).focus_handle(cx).is_focused(window);
        let theme = cx.theme();
        div()
            .key_context(COMMIT_KEY_CONTEXT)
            .flex_none()
            .px(px(CONTENT_INSET))
            .pt(px(6.))
            .child(
                div()
                    // The resting height of `panel_search`, so every input
                    // row in the panel sits on the same line.
                    .min_h(px(30.))
                    .max_h(px(120.))
                    .rounded(crate::ui::rounding::CARD_RADIUS)
                    .border_1()
                    .border_color(if focused { theme.ring } else { theme.border })
                    .bg(theme.input)
                    .px(px(8.))
                    .py(px(6.))
                    .child(Input::new(&input).appearance(false).xsmall()),
            )
            .into_any_element()
    }

    /// Hand the box the draft belonging to the repository on screen, and keep
    /// whatever is in it under the repository it was typed for.
    ///
    /// Per repository rather than per tab or per pane: a working tree has one
    /// pending message however many panes are looking at it.
    fn scm_commit_input(
        &mut self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        let input = match self.scm.commit_input.clone() {
            Some(input) => input,
            None => {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .multi_line(true)
                        .auto_grow(1, 6)
                        .placeholder(t(L10nKey::ScmCommitPlaceholder))
                });
                self.scm.commit_input = Some(input.clone());
                input
            }
        };
        let text = input.read(cx).value().to_string();

        if self.scm.commit_repo.as_ref() != Some(repo) {
            if let Some(previous) = self.scm.commit_repo.take() {
                self.scm.drafts.insert(previous, text);
            }
            let next = match self.scm.drafts.get(repo) {
                Some(draft) => draft.clone(),
                // A merge or a cherry-pick leaves git's own message in
                // `.git/MERGE_MSG`; starting from blank would throw away the
                // conflict summary the user is about to want.
                None => status.prefilled_message.clone().unwrap_or_default(),
            };
            input.update(cx, |state, cx| state.set_value(next, window, cx));
            self.scm.commit_repo = Some(repo.clone());
            return input;
        }

        if self.scm_commit_landed(repo, status, &text) {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            return input;
        }
        if self.scm.drafts.get(repo).map(String::as_str) != Some(text.as_str()) {
            self.scm.drafts.insert(repo.clone(), text);
        }
        input
    }

    /// Whether the commit we dispatched actually happened, and so whether the
    /// message may be thrown away.
    ///
    /// Clearing the box the moment `git commit` is *dispatched* would lose a
    /// carefully written message to a pre-commit hook that rejects it. HEAD
    /// moving is the one signal that says the message is now in the
    /// repository; an edit made in the meantime keeps it too.
    fn scm_commit_landed(
        &mut self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        text: &str,
    ) -> bool {
        let Some((sent_repo, before, message)) = &self.scm.committing else {
            return false;
        };
        if sent_repo != repo || *before == status.head || message != text {
            return false;
        }
        self.scm.committing = None;
        self.scm.drafts.remove(repo);
        true
    }

    fn scm_commit_buttons(
        &self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plan = commit_plan(status, self.scm.amend, self.scm.draft(repo));
        let repo_for_button = repo.clone();
        h_flex()
            .flex_none()
            .gap(px(4.))
            .px(px(CONTENT_INSET))
            .pt(px(6.))
            .pb(px(8.))
            .child(
                Button::new("scm-commit")
                    .primary()
                    .h(px(28.))
                    .flex_1()
                    .label(t(plan.label))
                    .disabled(!plan.enabled)
                    .when(!plan.enabled, |b| b.tooltip(t(L10nKey::ScmNothingToCommit)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.scm_commit(repo_for_button.clone(), this.scm.amend, window, cx);
                    })),
            )
            .child(self.scm_commit_menu(repo, cx))
            .into_any_element()
    }

    fn scm_commit_menu(&self, repo: &RepoKey, cx: &mut Context<Self>) -> AnyElement {
        let amend = self.scm.amend;
        crate::ui::tab_strip::chrome_tile_sized(
            Button::new("scm-commit-menu").icon(Icon::new(IconName::ChevronDown)),
            28.,
            12.,
            false,
            cx,
        )
        .rounded(crate::ui::rounding::CARD_RADIUS)
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, {
            let app = cx.entity().downgrade();
            let repo = repo.clone();
            move |menu, _window, _cx| {
                let mut menu = menu.min_w(px(190.));
                for (label, intent) in [
                    (L10nKey::ScmCommitButton, ScmIntent::Commit),
                    (L10nKey::ScmCommitAndPush, ScmIntent::CommitAndPush),
                    (L10nKey::ScmCommitAndSync, ScmIntent::CommitAndSync),
                ] {
                    menu = menu.item(PopupMenuItem::new(t(label)).on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ =
                                app.update(cx, |this, cx| this.run_scm_action(intent, window, cx));
                        }
                    }));
                }
                menu = menu.separator().item(
                    // A menu item rather than a checkbox row: the panel is
                    // 260px wide, and the armed state already shows up in the
                    // button's label and in the chip on the branch row.
                    PopupMenuItem::new(t(L10nKey::ScmAmendLastCommit))
                        .checked(amend)
                        .on_click({
                            let app = app.clone();
                            move |_, _window, cx| {
                                let _ = app.update(cx, |this, cx| {
                                    this.scm.amend = !this.scm.amend;
                                    cx.notify();
                                });
                            }
                        }),
                );
                menu.separator()
                    .item(PopupMenuItem::new(t(L10nKey::ScmStashAll)).on_click({
                        let app = app.clone();
                        let repo = repo.clone();
                        move |_, window, cx| {
                            let _ = app.update(cx, |this, cx| {
                                this.scm_stash_all(repo.clone(), window, cx)
                            });
                        }
                    }))
            }
        })
        .into_any_element()
    }

    /// Title over a scrolling body, with the panel's own scroll handle.
    ///
    /// Not `panel_scroll`: that one owns `right_panel.scroll`, and the rows
    /// that land between the title and the list in later steps have to stay
    /// pinned while the list moves under them.
    fn scm_shell(&self, title: AnyElement, body: AnyElement) -> AnyElement {
        self.scm_shell_with(title, Vec::new(), body)
    }

    /// `pinned` rows sit between the title and the list and do not scroll:
    /// the message box has to stay reachable however far down the files go.
    fn scm_shell_with(
        &self,
        title: AnyElement,
        pinned: Vec<AnyElement>,
        body: AnyElement,
    ) -> AnyElement {
        let scroller = div()
            .id("panel-scm-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scm.scroll)
            .child(body);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .children(pinned)
            .child(crate::ui::scrollbar::with_vertical_scrollbar(
                "panel-scm-scrollbar",
                scroller,
                &self.scm.scroll,
            ))
            .into_any_element()
    }

    /// The host and directory the panel is looking at.
    fn scm_pane_target(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(SharedHost, PathBuf)> {
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let cwd = view
            .git_status_cwd()
            .map(Path::to_path_buf)
            .or_else(|| view.host_cwd())?;
        Some((view.host(cx)?, cwd))
    }

    /// Turn the pane's directory into the repository root every write has to
    /// run from.
    ///
    /// Pathspecs out of `status --porcelain=v2` are relative to the root, so
    /// running `git add` from a subdirectory would name the wrong files. The
    /// root is also the cache key, which is what lets two panes in two
    /// subdirectories of one repository share a single status.
    ///
    /// The cheap repository/not-a-repository answer comes from the cache the
    /// tab badge already fills in, so a directory that is not a repository
    /// never reaches `git status` from here at all.
    fn scm_repo_root(
        &mut self,
        host: &SharedHost,
        cwd: &Path,
        cx: &mut Context<Self>,
    ) -> RepoLookup {
        let id = host.id();
        let key = (id, cwd.to_path_buf());
        if let Some(root) = self.scm.roots.get(&key) {
            return RepoLookup::Root(root.clone());
        }
        match cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.known_repo_for(id, cwd))
        {
            None => RepoLookup::Pending,
            Some(None) => RepoLookup::NotARepo,
            Some(Some(_)) => {
                self.scm_probe(host, cwd, cx);
                match status_of(cx, id, cwd) {
                    Some(status) => {
                        let root = status.root.clone();
                        self.scm.roots.insert(key, root.clone());
                        RepoLookup::Root(root)
                    }
                    None => RepoLookup::Pending,
                }
            }
        }
    }

    /// `scm_refresh` with a floor under how often a fruitless probe repeats.
    fn scm_probe(&mut self, host: &SharedHost, root: &Path, cx: &mut Context<Self>) {
        let key = (host.id(), root.to_path_buf());
        if status_of(cx, host.id(), root).is_none() {
            let now = Instant::now();
            match self.scm.probe_attempt.get(&key) {
                Some(at) if now.duration_since(*at) < PROBE_RETRY => return,
                _ => {
                    self.scm.probe_attempt.insert(key, now);
                }
            }
        }
        self.scm_refresh(host.clone(), root.to_path_buf(), cx);
    }

    /// Read the status and record which one this frame drew, so the watcher
    /// below can tell a real change from its own noise.
    fn scm_seen_status(
        &mut self,
        host: HostId,
        root: &Path,
        cx: &mut Context<Self>,
    ) -> Option<Arc<WorkingTreeStatus>> {
        let status = status_of(cx, host, root);
        self.scm.seen = Some((
            (host, root.to_path_buf()),
            status.as_ref().map_or(0, |s| Arc::as_ptr(s) as usize),
        ));
        status
    }

    /// Re-render when a probe lands.
    ///
    /// The subscription has to compare before it notifies. `scm_refresh`
    /// reaches for `ScmData` through `default_global`, which fires the global
    /// observers whether or not anything changed — and it is called from
    /// `render`. An unconditional `cx.notify()` here would therefore ask for a
    /// frame from inside a frame, forever.
    fn scm_watch_status(&mut self, cx: &mut Context<Self>) {
        if self.scm.watch.is_some() {
            return;
        }
        self.scm.watch = Some(cx.observe_global::<crate::terminal::git_data::ScmData>(
            |this, cx| {
                let Some((key, seen)) = this.scm.seen.clone() else {
                    return;
                };
                let now = status_of(cx, key.0, &key.1).map_or(0, |s| Arc::as_ptr(&s) as usize);
                if now != seen {
                    this.scm.seen = Some((key, now));
                    cx.notify();
                }
            },
        ));
    }

    fn scm_groups(
        &mut self,
        repo: &RepoKey,
        status: &Arc<WorkingTreeStatus>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().px(px(CONTENT_INSET - ROW_INSET)).py(px(2.));
        for group in ScmGroup::ORDER {
            let entries: Vec<&StatusEntry> = status
                .entries
                .iter()
                .filter(|e| in_group(e, group))
                .collect();
            if entries.is_empty() {
                continue;
            }
            let collapsed = self.scm.group_collapsed(group, entries.len());
            list = list.child(self.scm_group_header(repo, group, &entries, collapsed, cx));
            if collapsed {
                continue;
            }
            let shown = entries.len().min(MAX_RENDERED_FILES);
            for entry in entries.iter().take(shown) {
                list = list.child(self.scm_file_row(repo, group, entry, cx));
            }
            if entries.len() > shown {
                list = list.child(self.scm_note(
                    t_plural(L10nKey::PanelMoreChangedFiles, entries.len() - shown, &[]),
                    cx,
                ));
            }
        }
        if status.truncated {
            list = list.child(self.scm_note(
                t_fmt(
                    L10nKey::ScmTooManyChanges,
                    &[
                        ("shown", &status.entries.len().to_string()),
                        ("total", &status.total_entries.to_string()),
                    ],
                ),
                cx,
            ));
        }
        list.into_any_element()
    }

    fn scm_note(&self, text: String, cx: &mut Context<Self>) -> AnyElement {
        div()
            .px(px(ROW_INSET))
            .py(px(3.))
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground.opacity(0.75))
            .child(text)
            .into_any_element()
    }

    fn scm_group_header(
        &self,
        repo: &RepoKey,
        group: ScmGroup,
        entries: &[&StatusEntry],
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = entries.len();
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let id = SharedString::from(format!("scm-group-{group:?}"));
        let actions = self.scm_group_actions(&id, repo, group, entries, sf.hover, cx);
        h_flex()
            .id(id.clone())
            .group(id)
            .relative()
            .items_center()
            .gap(px(8.))
            .h(px(ROW_H))
            .px(px(ROW_INSET))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.scm_toggle_group(group, count, cx);
            }))
            // The chevron's box is exactly the width of `git_badge`, so this
            // column and the status letters below it are one straight line.
            .child(
                div()
                    .flex_none()
                    .w(px(BADGE_W))
                    .flex()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(px(11.)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(t(group_label(group)).to_uppercase()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .font_family(mono)
                    .text_color(cx.theme().muted_foreground.opacity(0.75))
                    .child(count.to_string()),
            )
            .child(actions)
            .into_any_element()
    }

    fn scm_file_row(
        &self,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let path = entry.path.as_str().to_string();
        let (name, dir) = split_display_path(&path);
        let (letter, deco) = row_status(entry, group);
        let selected = self.diff_overlay_focus(repo.host, &repo.root) == Some(path.as_str());
        let source = group_diff_source(group);
        let id = SharedString::from(format!("scm-row-{group:?}-{path}"));
        let actions = self.scm_row_actions(
            &id,
            repo,
            group,
            entry,
            if selected { sf.selected } else { sf.hover },
            cx,
        );

        h_flex()
            .id(id.clone())
            .group(id)
            .relative()
            .items_center()
            .gap(px(8.))
            .h(px(ROW_H))
            .px(px(ROW_INSET))
            .py(px(3.))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .when(selected, |s| s.bg(gpui::rgb(sf.selected)))
            .on_click({
                let repo = repo.clone();
                let path = path.clone();
                cx.listener(move |this, _, window, cx| {
                    this.open_diff_overlay(
                        repo.host,
                        repo.root.clone(),
                        source.clone(),
                        Some(path.clone()),
                        window,
                        cx,
                    );
                })
            })
            .context_menu({
                let app = cx.entity().downgrade();
                let repo = repo.clone();
                let entry = entry.clone();
                move |menu, _window, cx| {
                    Self::scm_row_context_menu(menu, &app, &repo, group, &entry, cx)
                }
            })
            .child(git_badge(letter, status_color(deco, cx), &mono))
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .font_family(mono.clone())
                    .text_color(if deco == DecoStatus::Deleted {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .when(deco == DecoStatus::Deleted, |s| s.line_through())
                    .child(name.to_string()),
            )
            // The directory gives way first: which file it is matters more
            // than where it lives, and the name is already the shorter half.
            .when(!dir.is_empty(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground.opacity(0.75))
                        .child(dir.to_string()),
                )
            })
            .child(actions)
            .into_any_element()
    }

    /// The buttons that appear over a hovered row.
    ///
    /// Absolutely positioned and opaque, so they cover the tail of the
    /// directory rather than pushing it aside: hovering a row must not move a
    /// single pixel of it, or the list crawls under the pointer.
    fn scm_row_actions(
        &self,
        row: &SharedString,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        backing: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A path git cannot be given is a path nothing can be done to. The
        // row stays readable and the buttons say why they are dead.
        let writable = entry.path.pathspec().is_some();
        let path = entry.path.as_str();
        let mut actions = h_flex()
            .occlude()
            .absolute()
            .right(px(ROW_INSET))
            .top_0()
            .bottom_0()
            .items_center()
            .gap(px(1.))
            .bg(gpui::rgb(backing))
            .invisible()
            .group_hover(row.clone(), |s| s.visible());

        for &(verb, ref icon) in row_verbs(group) {
            let id = SharedString::from(format!("scm-{verb:?}-{group:?}-{path}"));
            let repo = repo.clone();
            let entry = entry.clone();
            actions = actions.child(
                self.scm_tile(id, icon.clone(), verb_tooltip(verb), writable, cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.scm_row_verb(verb, &repo, group, &entry, window, cx);
                    })),
            );
        }
        actions.into_any_element()
    }

    fn scm_group_actions(
        &self,
        row: &SharedString,
        repo: &RepoKey,
        group: ScmGroup,
        entries: &[&StatusEntry],
        backing: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let paths = writable_paths(entries);
        let mut actions = h_flex()
            .occlude()
            .absolute()
            .right(px(ROW_INSET))
            .top_0()
            .bottom_0()
            .items_center()
            .gap(px(1.))
            .bg(gpui::rgb(backing))
            .invisible()
            .group_hover(row.clone(), |s| s.visible());

        for &(verb, ref icon) in group_verbs(group) {
            let id = SharedString::from(format!("scm-all-{verb:?}-{group:?}"));
            let repo = repo.clone();
            let paths = paths.clone();
            actions = actions.child(
                self.scm_tile(
                    id,
                    icon.clone(),
                    verb_all_tooltip(verb),
                    !paths.is_empty(),
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    let Some(op) = verb_op(verb, group, paths.clone()) else {
                        return;
                    };
                    this.scm_op(repo.clone(), op, window, cx);
                })),
            );
        }
        actions.into_any_element()
    }

    /// An 18px tile. Smaller than `TILE_SIZE_SM`, because three of those on a
    /// row would eat 72 of the 236px a file name has to live in.
    fn scm_tile(
        &self,
        id: SharedString,
        icon: IconName,
        tooltip: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        crate::ui::tab_strip::chrome_tile_sized(
            Button::new(id).icon(Icon::new(icon.clone())),
            TILE_SIZE_XS,
            TILE_GLYPH_XS,
            false,
            cx,
        )
        .rounded(px(4.))
        .disabled(!enabled)
        .tooltip(if enabled {
            tooltip
        } else {
            t(L10nKey::ScmUnrepresentablePath)
        })
    }

    fn scm_row_verb(
        &mut self,
        verb: RowVerb,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if verb == RowVerb::OpenConflict {
            self.open_diff_overlay(
                repo.host,
                repo.root.clone(),
                group_diff_source(group),
                Some(entry.path.as_str().to_string()),
                window,
                cx,
            );
            return;
        }
        let Some(op) = verb_op(verb, group, vec![entry.path.clone()]) else {
            return;
        };
        self.scm_op(repo.clone(), op, window, cx);
    }

    fn scm_row_context_menu(
        menu: PopupMenu,
        app: &gpui::WeakEntity<Self>,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        cx: &gpui::App,
    ) -> PopupMenu {
        let danger = cx.theme().danger;
        let rel = entry.path.as_str().to_string();
        let absolute = repo.root.join(&rel);
        let source = group_diff_source(group);
        let staged = group == ScmGroup::Staged;

        let mut menu = menu
            .min_w(px(200.))
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextOpen)).on_click({
                    let app = app.clone();
                    let absolute = absolute.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| {
                            this.open_file_in_editor(&absolute, window, cx)
                        });
                    }
                }),
            )
            .item(PopupMenuItem::new(t(L10nKey::ScmOpenChanges)).on_click({
                let app = app.clone();
                let repo = repo.clone();
                let rel = rel.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.open_diff_overlay(
                            repo.host,
                            repo.root.clone(),
                            source.clone(),
                            Some(rel.clone()),
                            window,
                            cx,
                        );
                    });
                }
            }))
            .separator()
            .item(
                PopupMenuItem::new(if staged {
                    t(L10nKey::ScmUnstage)
                } else {
                    t(L10nKey::ScmStage)
                })
                .on_click({
                    let app = app.clone();
                    let repo = repo.clone();
                    let paths = vec![entry.path.clone()];
                    move |_, window, cx| {
                        let op = if staged {
                            GitOp::Unstage {
                                paths: paths.clone(),
                            }
                        } else {
                            GitOp::Stage {
                                paths: paths.clone(),
                            }
                        };
                        let _ =
                            app.update(cx, |this, cx| this.scm_op(repo.clone(), op, window, cx));
                    }
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextCopyPath)).on_click({
                    let absolute = absolute.clone();
                    move |_, _window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            absolute.display().to_string(),
                        ));
                    }
                }),
            );

        // Revealing a path only means anything on the machine the window is
        // running on; a remote repository's paths are not this filesystem's.
        if repo.host == HostId::LOCAL {
            menu = menu.item(
                PopupMenuItem::new(crate::ui::right_panel::reveal_label()).on_click({
                    let absolute = absolute.clone();
                    move |_, _window, cx| cx.reveal_path(&absolute)
                }),
            );
        }

        if let Some(op) = verb_op(RowVerb::Discard, group, vec![entry.path.clone()]) {
            menu = menu.separator().item(
                PopupMenuItem::element(move |_window, _cx| {
                    div().text_color(danger).child(t(L10nKey::ScmDiscard))
                })
                .on_click({
                    let app = app.clone();
                    let repo = repo.clone();
                    move |_, window, cx| {
                        let op = op.clone();
                        let _ =
                            app.update(cx, |this, cx| this.scm_op(repo.clone(), op, window, cx));
                    }
                }),
            );
        }
        menu
    }

    /// What `app.rs`'s `GitStatusCache` observer calls when the cheap
    /// per-tab probe lands.
    ///
    /// That probe runs at every command boundary, which makes it the best
    /// signal there is that the working tree moved — far better than a timer.
    /// The comparison in front of the bump is what keeps it from turning every
    /// notification (including the one the probe's *start* fires) into another
    /// `git status`.
    pub(crate) fn right_panel_refresh_changes(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.scm.repo.clone() else {
            return;
        };
        let Some(seen) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.status_for(repo.host, &repo.root))
        else {
            return;
        };
        if self.scm.last_tab_status.as_ref() == Some(&seen) {
            return;
        }
        self.scm.last_tab_status = Some(seen);
        self.scm_invalidate(&repo, cx);
    }

    /// Send the next look at a repository back to git.
    pub(crate) fn scm_invalidate(&mut self, repo: &RepoKey, cx: &mut Context<Self>) {
        cx.default_global::<crate::terminal::git_data::ScmData>()
            .bump(repo.host, &repo.root);
        cx.notify();
    }

    fn scm_toggle_group(&mut self, group: ScmGroup, count: usize, cx: &mut Context<Self>) {
        let collapsed = self.scm.group_collapsed(group, count);
        self.scm.set_group_collapsed(group, !collapsed);
        cx.notify();
    }
}

/// Which sections an entry shows up in.
///
/// A file can be staged and unstaged at once (`XY == "MM"`), and then it
/// belongs in both — the same thing VS Code shows, and the honest reading of
/// what a commit right now would contain.
pub(crate) fn in_group(entry: &StatusEntry, group: ScmGroup) -> bool {
    match group {
        ScmGroup::Merge => entry.is_conflicted(),
        ScmGroup::Untracked => entry.is_untracked(),
        ScmGroup::Staged => entry.is_staged(),
        ScmGroup::Changes => entry.is_unstaged() && !entry.is_untracked(),
    }
}

/// The letter and colour a row wears, decided by the half of `XY` its group
/// is about — a file staged as added and then modified is `A` under Staged
/// and `M` under Changes, which is what `git status` itself says.
pub(crate) fn row_status(entry: &StatusEntry, group: ScmGroup) -> (&'static str, DecoStatus) {
    let deco = match group {
        ScmGroup::Merge => DecoStatus::Conflict,
        ScmGroup::Untracked => DecoStatus::Untracked,
        ScmGroup::Staged => code_deco(entry.index),
        ScmGroup::Changes => code_deco(entry.worktree),
    };
    let letter = match group {
        // The two groups whose letter is fixed use the shared glyph table, so
        // a conflict is `U` here and in the file tree alike.
        ScmGroup::Merge | ScmGroup::Untracked => status_glyph(deco),
        ScmGroup::Staged => letter_of(entry.index),
        ScmGroup::Changes => letter_of(entry.worktree),
    };
    (letter, deco)
}

/// `ChangeCode::letter` returns a `char`; rows want a `&'static str` so the
/// badge never allocates. `T` and `C` keep their own letters rather than being
/// folded into `M` and `R` — git shows them, and they mean different things.
fn letter_of(code: ChangeCode) -> &'static str {
    match code {
        ChangeCode::None => " ",
        ChangeCode::Modified => "M",
        ChangeCode::TypeChanged => "T",
        ChangeCode::Added => "A",
        ChangeCode::Deleted => "D",
        ChangeCode::Renamed => "R",
        ChangeCode::Copied => "C",
        ChangeCode::Unmerged => "U",
    }
}

fn code_deco(code: ChangeCode) -> DecoStatus {
    match code {
        ChangeCode::Deleted => DecoStatus::Deleted,
        ChangeCode::Added => DecoStatus::Added,
        ChangeCode::Renamed | ChangeCode::Copied => DecoStatus::Renamed,
        ChangeCode::Unmerged => DecoStatus::Conflict,
        _ => DecoStatus::Modified,
    }
}

/// Which patch a row's click opens.
///
/// Staged rows show `git diff --cached`; everything else shows the working
/// tree. Getting this wrong is not cosmetic — the file name would be right and
/// the hunks underneath it would be someone else's.
pub(crate) fn group_diff_source(group: ScmGroup) -> DiffSource {
    match group {
        ScmGroup::Staged => DiffSource::Staged,
        ScmGroup::Merge | ScmGroup::Changes | ScmGroup::Untracked => DiffSource::Worktree,
    }
}

fn group_label(group: ScmGroup) -> L10nKey {
    match group {
        ScmGroup::Merge => L10nKey::ScmGroupMerge,
        ScmGroup::Staged => L10nKey::ScmGroupStaged,
        ScmGroup::Changes => L10nKey::ScmGroupChanges,
        ScmGroup::Untracked => L10nKey::ScmGroupUntracked,
    }
}

/// Whether a group nobody has touched starts folded.
pub(crate) fn starts_collapsed(group: ScmGroup, count: usize) -> bool {
    group == ScmGroup::Untracked && count > UNTRACKED_AUTO_COLLAPSE
}

/// What the commit button says, and whether it can be pressed at all.
pub(crate) struct CommitPlan {
    pub(crate) label: L10nKey,
    pub(crate) enabled: bool,
}

/// Decide both from the state of the index.
///
/// "Commit All" rather than a silent `-a`: with nothing staged, `git commit`
/// would commit nothing, and the honest thing is to say on the button that
/// every tracked change is about to go in. An armed amend needs no message —
/// `--no-edit` keeps the one that is already on HEAD.
pub(crate) fn commit_plan(status: &WorkingTreeStatus, amend: bool, message: &str) -> CommitPlan {
    let staged = status.staged().next().is_some();
    let tracked_edits = status.unstaged().next().is_some();
    let label = if amend {
        L10nKey::ScmCommitAmendButton
    } else if staged {
        L10nKey::ScmCommitButton
    } else {
        L10nKey::ScmCommitAllButton
    };
    let has_message = !message.trim().is_empty();
    CommitPlan {
        label,
        enabled: (staged || tracked_edits || amend) && (has_message || amend),
    }
}

/// Whether a commit has to stage everything tracked first (`-a`).
pub(crate) fn commit_stages_everything(status: &WorkingTreeStatus, amend: bool) -> bool {
    // Amending with nothing staged means "fix the message", not "sweep the
    // working tree into the commit I already made".
    !amend && status.staged().next().is_none()
}

/// What a row's buttons do. Named rather than inlined because the same verb
/// appears on the row, on its group header and in its context menu, and the
/// three must not drift into meaning different things.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowVerb {
    Discard,
    Stage,
    Unstage,
    OpenConflict,
    MarkResolved,
}

/// Right to left, most-used last: the pointer travels to the right edge, so
/// the button under it should be the one nine hovers out of ten want.
pub(crate) fn row_verbs(group: ScmGroup) -> &'static [(RowVerb, IconName)] {
    match group {
        ScmGroup::Merge => &[
            (RowVerb::OpenConflict, IconName::Eye),
            (RowVerb::MarkResolved, IconName::Check),
        ],
        ScmGroup::Staged => &[(RowVerb::Unstage, IconName::Minus)],
        ScmGroup::Changes | ScmGroup::Untracked => &[
            (RowVerb::Discard, IconName::Undo2),
            (RowVerb::Stage, IconName::Plus),
        ],
    }
}

/// The header's buttons are the row's, minus the ones that only make sense
/// for one file: there is no group-wide "open the conflict".
pub(crate) fn group_verbs(group: ScmGroup) -> &'static [(RowVerb, IconName)] {
    match group {
        ScmGroup::Merge => &[(RowVerb::MarkResolved, IconName::Check)],
        _ => row_verbs(group),
    }
}

/// The operation a verb runs over one or many paths. `None` for the verbs
/// that change no state.
pub(crate) fn verb_op(verb: RowVerb, group: ScmGroup, paths: Vec<RepoPath>) -> Option<GitOp> {
    if paths.is_empty() {
        return None;
    }
    Some(match verb {
        // Resolving a conflict is `git add`, exactly as it is on the command
        // line — there is no separate "resolve" verb in git.
        RowVerb::Stage | RowVerb::MarkResolved => GitOp::Stage { paths },
        RowVerb::Unstage => GitOp::Unstage { paths },
        RowVerb::Discard if group == ScmGroup::Untracked => {
            let directories = paths.iter().any(|p| p.as_str().ends_with('/'));
            GitOp::DiscardUntracked { paths, directories }
        }
        RowVerb::Discard => GitOp::DiscardWorktree { paths },
        RowVerb::OpenConflict => return None,
    })
}

/// The paths in a group git can actually be told about.
///
/// A path that is not valid UTF-8 cannot be sent as a pathspec at all, and
/// `GitOp::validate` rejects the whole operation over one of them — so a group
/// action has to leave them out rather than fail for everyone. Their own rows
/// are greyed out and say why.
pub(crate) fn writable_paths(entries: &[&StatusEntry]) -> Vec<RepoPath> {
    entries
        .iter()
        .filter(|e| e.path.pathspec().is_some())
        .map(|e| e.path.clone())
        .collect()
}

fn verb_tooltip(verb: RowVerb) -> &'static str {
    t(match verb {
        RowVerb::Discard => L10nKey::ScmDiscard,
        RowVerb::Stage => L10nKey::ScmStage,
        RowVerb::Unstage => L10nKey::ScmUnstage,
        RowVerb::OpenConflict => L10nKey::ScmOpenConflict,
        RowVerb::MarkResolved => L10nKey::ScmMarkResolved,
    })
}

fn verb_all_tooltip(verb: RowVerb) -> &'static str {
    t(match verb {
        RowVerb::Discard => L10nKey::ScmDiscardAll,
        RowVerb::Stage => L10nKey::ScmStageAll,
        RowVerb::Unstage => L10nKey::ScmUnstageAll,
        RowVerb::OpenConflict => L10nKey::ScmOpenConflict,
        RowVerb::MarkResolved => L10nKey::ScmMarkResolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::actions::{ScmCommit, ToggleFullscreen};
    use crate::core::config::{CoreConfig, DiffViewMode, RightPanelTab};
    use crate::ui::app::test_window::harness;
    use gpui::TestAppContext;
    use tty7_core::core::git::status::{ConflictKind, EntryKind, HeadState, RepoPath};

    fn entry(path: &str, index: ChangeCode, worktree: ChangeCode, kind: EntryKind) -> StatusEntry {
        StatusEntry {
            path: RepoPath::from_bytes(path.as_bytes()),
            orig_path: None,
            index,
            worktree,
            kind,
            submodule: None,
            rename_score: None,
            conflict: (kind == EntryKind::Unmerged).then_some(ConflictKind::BothModified),
        }
    }

    fn groups_of(entry: &StatusEntry) -> Vec<ScmGroup> {
        ScmGroup::ORDER
            .into_iter()
            .filter(|g| in_group(entry, *g))
            .collect()
    }

    #[test]
    fn a_file_staged_and_edited_again_lands_in_both_groups() {
        let e = entry(
            "a.rs",
            ChangeCode::Modified,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        assert_eq!(groups_of(&e), vec![ScmGroup::Staged, ScmGroup::Changes]);
    }

    #[test]
    fn each_other_kind_of_entry_lands_in_exactly_one_group() {
        let staged = entry(
            "a.rs",
            ChangeCode::Added,
            ChangeCode::None,
            EntryKind::Tracked,
        );
        assert_eq!(groups_of(&staged), vec![ScmGroup::Staged]);

        let unstaged = entry(
            "b.rs",
            ChangeCode::None,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        assert_eq!(groups_of(&unstaged), vec![ScmGroup::Changes]);

        let untracked = entry(
            "c.rs",
            ChangeCode::None,
            ChangeCode::None,
            EntryKind::Untracked,
        );
        assert_eq!(groups_of(&untracked), vec![ScmGroup::Untracked]);

        // A conflict is only ever a conflict: it must not also show up under
        // Changes, or resolving it would look like two separate jobs.
        let conflict = entry(
            "d.rs",
            ChangeCode::Unmerged,
            ChangeCode::Unmerged,
            EntryKind::Unmerged,
        );
        assert_eq!(groups_of(&conflict), vec![ScmGroup::Merge]);
    }

    #[test]
    fn a_row_wears_the_letter_of_the_half_its_group_is_about() {
        // Added to the index, then edited again in the working tree.
        let e = entry(
            "a.rs",
            ChangeCode::Added,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        assert_eq!(row_status(&e, ScmGroup::Staged), ("A", DecoStatus::Added));
        assert_eq!(
            row_status(&e, ScmGroup::Changes),
            ("M", DecoStatus::Modified)
        );

        let untracked = entry(
            "c.rs",
            ChangeCode::None,
            ChangeCode::None,
            EntryKind::Untracked,
        );
        assert_eq!(
            row_status(&untracked, ScmGroup::Untracked),
            ("?", DecoStatus::Untracked)
        );
        let conflict = entry(
            "d.rs",
            ChangeCode::Unmerged,
            ChangeCode::Unmerged,
            EntryKind::Unmerged,
        );
        assert_eq!(
            row_status(&conflict, ScmGroup::Merge),
            ("U", DecoStatus::Conflict)
        );
    }

    #[test]
    fn staged_rows_open_the_cached_diff_and_the_rest_the_working_tree() {
        assert_eq!(group_diff_source(ScmGroup::Staged), DiffSource::Staged);
        for group in [ScmGroup::Merge, ScmGroup::Changes, ScmGroup::Untracked] {
            assert_eq!(group_diff_source(group), DiffSource::Worktree);
        }
    }

    #[test]
    fn only_a_long_untracked_list_starts_folded() {
        assert!(!starts_collapsed(
            ScmGroup::Untracked,
            UNTRACKED_AUTO_COLLAPSE
        ));
        assert!(starts_collapsed(
            ScmGroup::Untracked,
            UNTRACKED_AUTO_COLLAPSE + 1
        ));
        for group in [ScmGroup::Merge, ScmGroup::Staged, ScmGroup::Changes] {
            assert!(!starts_collapsed(group, 1_000));
        }
    }

    #[test]
    fn every_button_a_row_offers_maps_to_the_verb_it_is_named_for() {
        let file = vec![RepoPath::from_bytes(b"a.rs")];
        assert!(matches!(
            verb_op(RowVerb::Stage, ScmGroup::Changes, file.clone()),
            Some(GitOp::Stage { .. })
        ));
        assert!(matches!(
            verb_op(RowVerb::Unstage, ScmGroup::Staged, file.clone()),
            Some(GitOp::Unstage { .. })
        ));
        // Resolving a conflict is `git add`; git has no other verb for it.
        assert!(matches!(
            verb_op(RowVerb::MarkResolved, ScmGroup::Merge, file.clone()),
            Some(GitOp::Stage { .. })
        ));
        assert!(matches!(
            verb_op(RowVerb::Discard, ScmGroup::Changes, file.clone()),
            Some(GitOp::DiscardWorktree { .. })
        ));
        // `checkout --` cannot restore a file git has never heard of.
        assert!(matches!(
            verb_op(RowVerb::Discard, ScmGroup::Untracked, file.clone()),
            Some(GitOp::DiscardUntracked {
                directories: false,
                ..
            })
        ));
        assert!(matches!(
            verb_op(
                RowVerb::Discard,
                ScmGroup::Untracked,
                vec![RepoPath::from_bytes(b"vendor/")]
            ),
            Some(GitOp::DiscardUntracked {
                directories: true,
                ..
            })
        ));
        assert!(verb_op(RowVerb::OpenConflict, ScmGroup::Merge, file).is_none());
        assert!(verb_op(RowVerb::Stage, ScmGroup::Changes, Vec::new()).is_none());
    }

    #[test]
    fn everything_that_can_lose_work_says_so_before_it_runs() {
        // The gate in `scm_op` keys off `destructive()`. If one of these ever
        // stopped reporting, the panel would throw the work away in silence.
        let paths = vec![RepoPath::from_bytes(b"a.rs")];
        for group in [ScmGroup::Changes, ScmGroup::Untracked] {
            let op = verb_op(RowVerb::Discard, group, paths.clone()).expect("discard has an op");
            assert!(op.destructive().is_some(), "{group:?} discard");
        }
        // Staging and unstaging are reversible, so they must not stop to ask.
        for verb in [RowVerb::Stage, RowVerb::Unstage, RowVerb::MarkResolved] {
            let op = verb_op(verb, ScmGroup::Changes, paths.clone()).expect("verb has an op");
            assert!(op.destructive().is_none(), "{verb:?}");
        }
    }

    #[test]
    fn a_group_action_leaves_out_the_paths_git_cannot_be_told_about() {
        let good = entry(
            "a.rs",
            ChangeCode::None,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        let mut bad = good.clone();
        bad.path = RepoPath::from_bytes(&[0xff, 0xfe, b'.', b'r', b's']);
        assert!(bad.path.pathspec().is_none(), "the fixture must be lossy");

        let paths = writable_paths(&[&good, &bad]);
        assert_eq!(paths.len(), 1, "the lossy path is dropped, not carried");
        // One unrepresentable path would otherwise fail the operation for the
        // whole group.
        let op = verb_op(RowVerb::Stage, ScmGroup::Changes, paths).expect("still has work to do");
        assert!(op.validate().is_ok());

        let all_bad = verb_op(RowVerb::Stage, ScmGroup::Changes, writable_paths(&[&bad]));
        assert!(all_bad.is_none(), "nothing to do means no operation at all");
    }

    #[test]
    fn a_group_header_offers_no_button_that_only_makes_sense_for_one_file() {
        let verbs: Vec<RowVerb> = group_verbs(ScmGroup::Merge)
            .iter()
            .map(|(v, _)| *v)
            .collect();
        assert_eq!(verbs, vec![RowVerb::MarkResolved]);
        for group in [ScmGroup::Staged, ScmGroup::Changes, ScmGroup::Untracked] {
            for (verb, _) in group_verbs(group) {
                assert_ne!(*verb, RowVerb::OpenConflict);
            }
        }
    }

    fn repo(root: &str) -> RepoKey {
        RepoKey {
            host: HostId::LOCAL,
            root: PathBuf::from(root),
        }
    }

    fn status_of_repo(root: &str, entries: Vec<StatusEntry>) -> WorkingTreeStatus {
        WorkingTreeStatus {
            root: PathBuf::from(root),
            home: PathBuf::from(root),
            head: HeadState::Branch {
                name: "main".into(),
                oid: "1111111".into(),
            },
            upstream: None,
            ahead_behind: None,
            total_entries: entries.len(),
            entries,
            truncated: false,
            stash_count: 0,
            operation: None,
            prefilled_message: None,
        }
    }

    #[test]
    fn the_commit_button_says_what_pressing_it_would_actually_do() {
        let staged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::Modified,
                ChangeCode::None,
                EntryKind::Tracked,
            )],
        );
        let unstaged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::None,
                ChangeCode::Modified,
                EntryKind::Tracked,
            )],
        );
        let clean = status_of_repo("/a", Vec::new());

        assert_eq!(
            commit_plan(&staged, false, "msg").label,
            L10nKey::ScmCommitButton
        );
        // Nothing staged: the button says so rather than quietly running -a.
        assert_eq!(
            commit_plan(&unstaged, false, "msg").label,
            L10nKey::ScmCommitAllButton
        );
        assert_eq!(
            commit_plan(&staged, true, "").label,
            L10nKey::ScmCommitAmendButton
        );

        assert!(commit_plan(&staged, false, "msg").enabled);
        assert!(
            !commit_plan(&staged, false, "   ").enabled,
            "an all-whitespace message is no message"
        );
        assert!(
            commit_plan(&clean, true, "").enabled,
            "amending with no message keeps HEAD's own with --no-edit"
        );
        assert!(!commit_plan(&clean, false, "msg").enabled);
    }

    #[test]
    fn only_a_commit_with_an_empty_index_sweeps_the_working_tree() {
        let staged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::Modified,
                ChangeCode::None,
                EntryKind::Tracked,
            )],
        );
        let unstaged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::None,
                ChangeCode::Modified,
                EntryKind::Tracked,
            )],
        );
        assert!(commit_stages_everything(&unstaged, false));
        assert!(!commit_stages_everything(&staged, false));
        // Amending is "fix the last commit", not "add everything to it".
        assert!(!commit_stages_everything(&unstaged, true));
    }

    #[gpui::test]
    fn commit_action_is_scoped_to_the_message_box(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (_app, mut vcx) = harness(cx);

        let (in_box, outside, window_wide) = vcx.update(|window, _cx| {
            let scoped = gpui::KeyContext::parse(COMMIT_KEY_CONTEXT)
                .expect("the context the panel installs parses");
            (
                window.bindings_for_action_in_context(&ScmCommit, scoped),
                window.bindings_for_action_in_context(
                    &ScmCommit,
                    gpui::KeyContext::new_with_defaults(),
                ),
                window.bindings_for_action_in_context(
                    &ToggleFullscreen,
                    gpui::KeyContext::new_with_defaults(),
                ),
            )
        });

        assert!(
            !in_box.is_empty(),
            "the message box installs {COMMIT_KEY_CONTEXT}, and the binding has to live in it"
        );
        assert!(
            outside.is_empty(),
            "outside the box the chord must not commit anything"
        );
        assert!(
            !window_wide.is_empty(),
            "the window keeps its own binding for the same chord"
        );
        if cfg!(target_os = "macos") {
            // The whole point: one chord, two meanings, separated only by the
            // context the box installs. If they ever stopped colliding this
            // test would still pass for the wrong reason, so assert they do.
            assert_eq!(
                in_box[0].keystrokes(),
                window_wide[0].keystrokes(),
                "secondary-enter is shared, and scope is what tells them apart"
            );
        }
    }

    #[gpui::test]
    fn a_commit_draft_follows_its_repository(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        let (a, b) = (repo("/a"), repo("/b"));
        let status_a = status_of_repo("/a", Vec::new());
        let status_b = status_of_repo("/b", Vec::new());

        app.update_in(&mut vcx, |app, window, cx| {
            let input = app.scm_commit_input(&a, &status_a, window, cx);
            input.update(cx, |state, cx| state.set_value("wip: a", window, cx));

            let input = app.scm_commit_input(&b, &status_b, window, cx);
            assert_eq!(
                input.read(cx).value(),
                "",
                "another working tree is another message"
            );
            input.update(cx, |state, cx| state.set_value("wip: b", window, cx));

            let input = app.scm_commit_input(&a, &status_a, window, cx);
            assert_eq!(
                input.read(cx).value(),
                "wip: a",
                "coming back has to bring the draft with it"
            );
            assert_eq!(app.scm.draft(&b), "wip: b");
        });
    }

    #[gpui::test]
    fn a_merge_prefills_the_box_with_gits_own_message(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        let key = repo("/a");
        let mut status = status_of_repo("/a", Vec::new());
        status.prefilled_message = Some("Merge branch 'topic'".into());

        app.update_in(&mut vcx, |app, window, cx| {
            let input = app.scm_commit_input(&key, &status, window, cx);
            assert_eq!(input.read(cx).value(), "Merge branch 'topic'");
        });
    }

    #[gpui::test]
    fn a_rejected_commit_keeps_the_message_it_was_given(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        let key = repo("/a");
        let before = status_of_repo("/a", Vec::new());

        app.update(&mut vcx, |app, _cx| {
            app.scm.committing = Some((key.clone(), before.head.clone(), "wip".into()));

            // A pre-commit hook said no: HEAD has not moved, so the message
            // the user wrote is still the only copy of it there is.
            assert!(!app.scm_commit_landed(&key, &before, "wip"));

            let mut after = before.clone();
            after.head = HeadState::Branch {
                name: "main".into(),
                oid: "2222222".into(),
            };
            assert!(app.scm_commit_landed(&key, &after, "wip"));
            assert_eq!(app.scm.draft(&key), "");
        });
    }

    fn tab_from(json: &str) -> RightPanelTab {
        serde_json::from_str::<CoreConfig>(json)
            .expect("config deserializes")
            .right_panel_tab
    }

    #[gpui::test]
    fn scm_tab_opens_and_config_round_trips(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        app.update(&mut vcx, |app, cx| {
            app.set_right_panel_tab(RightPanelTab::Scm, cx)
        });
        let (visible, tab) = app.read_with(&vcx, |app, _| {
            (app.right_panel_visible, app.right_panel_tab)
        });
        assert!(visible);
        assert_eq!(tab, RightPanelTab::Scm);

        // The whole reason the variant was renamed in place: what lands on
        // disk is still `"changes"`, so a build from before this change reads
        // its own config back and stays on the panel the user left open.
        let cfg = CoreConfig {
            right_panel_tab: RightPanelTab::Scm,
            ..Default::default()
        };
        let json = serde_json::to_value(&cfg).expect("config serializes");
        assert_eq!(json["right_panel_tab"], serde_json::json!("changes"));

        assert_eq!(
            tab_from(r#"{"right_panel_tab":"changes"}"#),
            RightPanelTab::Scm
        );
        assert_eq!(tab_from(r#"{"right_panel_tab":"scm"}"#), RightPanelTab::Scm);
        assert_eq!(tab_from(r#"{"right_panel_tab":"git"}"#), RightPanelTab::Scm);
        // Anything unrecognised falls back through `de_lenient` rather than
        // failing the whole file.
        assert_eq!(
            tab_from(r#"{"right_panel_tab":"nonsense"}"#),
            RightPanelTab::Info
        );
    }

    #[gpui::test]
    fn the_new_config_fields_default_to_todays_behaviour(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        let cfg = CoreConfig::default();
        assert_eq!(cfg.diff_view, DiffViewMode::Split);
        assert!(!cfg.scm_graph_expanded);

        // Toggling has to survive the round trip through the config, since the
        // panel reads it back on the next launch.
        app.update(&mut vcx, |app, cx| app.scm_toggle_graph(cx));
        assert!(app.read_with(&vcx, |app, _| app.scm.graph.expanded));
        assert!(vcx.update(|_, cx| {
            cx.global::<crate::core::config::Config>()
                .scm_graph_expanded
        }));

        app.update(&mut vcx, |app, cx| app.toggle_diff_view_mode(cx));
        assert_eq!(
            vcx.update(|_, cx| cx.global::<crate::core::config::Config>().diff_view),
            DiffViewMode::Unified
        );
    }

    #[gpui::test]
    fn a_folded_group_stays_folded_across_rerenders(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        app.update(&mut vcx, |app, cx| {
            app.set_right_panel_tab(RightPanelTab::Scm, cx);
            app.scm_toggle_group(ScmGroup::Staged, 3, cx);
        });
        vcx.background_executor.run_until_parked();
        vcx.run_until_parked();
        assert!(app.read_with(&vcx, |app, _| app.scm.group_collapsed(ScmGroup::Staged, 3)));

        // And a long untracked list that the user opened by hand stays open,
        // rather than snapping shut again on the count.
        app.update(&mut vcx, |app, cx| {
            app.scm_toggle_group(ScmGroup::Untracked, 500, cx)
        });
        vcx.run_until_parked();
        assert!(!app.read_with(&vcx, |app, _| {
            app.scm.group_collapsed(ScmGroup::Untracked, 500)
        }));
    }
}
