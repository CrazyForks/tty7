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

use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*, px};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};

use tty7_core::core::git::diff::MAX_RENDERED_FILES;
use tty7_core::core::git::status::{ChangeCode, DecoStatus, StatusEntry, WorkingTreeStatus};

use crate::terminal::git_data::status_of;
use crate::terminal::git_diff::DiffSource;
use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::host_ops::{HostId, SharedHost};
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::right_panel::git_badge;
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

        self.scm.repo = Some(RepoKey {
            host: host.id(),
            root: root.clone(),
        });

        let count = (status.total_entries > 0).then(|| status.total_entries.to_string());
        let title = self.panel_title(t(L10nKey::PanelScmTitle), count, None, window, cx);

        if status.is_clean() {
            let body = self.panel_empty(
                t(L10nKey::PanelNoChanges),
                Some(t(L10nKey::PanelNoChangesHint)),
                cx,
            );
            return self.scm_shell(title, body);
        }

        let body = self.scm_groups(&host, &root, &status, cx);
        self.scm_shell(title, body)
    }

    /// Title over a scrolling body, with the panel's own scroll handle.
    ///
    /// Not `panel_scroll`: that one owns `right_panel.scroll`, and the rows
    /// that land between the title and the list in later steps have to stay
    /// pinned while the list moves under them.
    fn scm_shell(&self, title: AnyElement, body: AnyElement) -> AnyElement {
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
        host: &SharedHost,
        root: &Path,
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
            list = list.child(self.scm_group_header(group, entries.len(), collapsed, cx));
            if collapsed {
                continue;
            }
            let shown = entries.len().min(MAX_RENDERED_FILES);
            for entry in entries.iter().take(shown) {
                list = list.child(self.scm_file_row(host, root, group, entry, cx));
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
        group: ScmGroup,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        h_flex()
            .id(SharedString::from(format!("scm-group-{group:?}")))
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
            .into_any_element()
    }

    fn scm_file_row(
        &self,
        host: &SharedHost,
        root: &Path,
        group: ScmGroup,
        entry: &StatusEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let path = entry.path.as_str().to_string();
        let (name, dir) = split_display_path(&path);
        let (letter, deco) = row_status(entry, group);
        let selected = self.diff_overlay_focus(host.id(), root) == Some(path.as_str());
        let source = group_diff_source(group);

        h_flex()
            .id(SharedString::from(format!("scm-row-{group:?}-{path}")))
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
                let host_id = host.id();
                let root = root.to_path_buf();
                let path = path.clone();
                cx.listener(move |this, _, window, cx| {
                    this.open_diff_overlay(
                        host_id,
                        root.clone(),
                        source.clone(),
                        Some(path.clone()),
                        window,
                        cx,
                    );
                })
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
            .into_any_element()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{CoreConfig, DiffViewMode, RightPanelTab};
    use crate::ui::app::test_window::harness;
    use gpui::TestAppContext;
    use tty7_core::core::git::status::{ConflictKind, EntryKind, RepoPath};

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
