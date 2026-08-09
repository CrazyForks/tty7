//! One commit, in the panel's own body.
//!
//! Replacing the body rather than opening a third kind of container: the
//! panel already knows how to draw a list of changed files, and the only
//! things a commit adds above it are its message and who wrote it. The file
//! rows are the same rows, minus the buttons — nothing here should be able to
//! drift from what the working tree shows.
//!
//! The patch itself still goes to the full-screen overlay. 260px is not a
//! place to read a diff.
//!
//! This is also where the panel pays back what the graph gave up. A history
//! row has about 26 characters beside its lanes and this repository's subjects
//! run to a median of 64, so the graph shows shape and this shows text: the
//! whole subject, the body, every ref, the parents, and the files.

use std::sync::Arc;

use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*, px};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use tty7_core::core::git::diff::CommitLabel;
use tty7_core::core::git::log::{Commit, CommitFile, RefKind};
use tty7_core::core::git::status::DecoStatus;

use crate::terminal::git_diff::DiffSource;
use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_plural};
use crate::ui::right_panel::{git_badge, info_chip};
use crate::ui::scm::path::{relative_time, split_display_path};
use crate::ui::scm::state::{CommitDetailView, RepoKey};
use crate::ui::scm::status::{status_color, status_glyph};

/// A file row, the same height as the working tree's, and inset the same way.
/// The two lists sit in one column and have to read as one grid.
const ROW_H: f32 = 24.;
const ROW_INSET: f32 = 4.;

/// How much of the body is shown before it folds. Four lines is a paragraph;
/// past that it is a changelog, and the file list is what the reader came for.
const BODY_LINES: usize = 4;

/// And how much of the subject, which wraps rather than folding. Three lines
/// of 12px in 260px is around 90 characters — longer than every subject in
/// this repository but a handful, and a cap for the ones that are a paragraph.
const SUBJECT_LINES: usize = 3;

impl Tty7App {
    /// Show one commit, replacing the working tree in the panel body.
    ///
    /// `seed` is the commit the caller already has. The graph's page carries
    /// every field this view renders, so a click on a row hands its own
    /// [`Commit`] over and no `git show` is run at all; a parent link, or
    /// anything else reaching a commit outside that window, passes `None` and
    /// pays for the read.
    pub(crate) fn open_commit_detail(
        &mut self,
        repo: RepoKey,
        oid: String,
        seed: Option<Commit>,
        cx: &mut Context<Self>,
    ) {
        self.scm.detail = Some(CommitDetailView::new(repo, oid, seed));
        cx.notify();
    }

    pub(crate) fn close_commit_detail(&mut self, cx: &mut Context<Self>) {
        if self.scm.detail.take().is_some() {
            cx.notify();
        }
    }

    /// The commit detail body, shown in place of the file groups.
    pub(crate) fn render_commit_detail(
        &mut self,
        detail: &CommitDetailView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // The detail names its own repository, and the panel may since have
        // followed the active pane somewhere else. A commit from a repository
        // nobody is looking at any more is not a second-level view of
        // anything, so it goes rather than sitting on top of the wrong tree.
        if self.scm.active_repo() != Some(&detail.repo) {
            self.scm.detail = None;
            return None;
        }
        self.load_commit_detail(detail, cx);

        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        // Each section insets itself rather than sharing one on the column:
        // `panel_subtitle` applies `CONTENT_INSET` of its own, and an outer
        // inset would push it eight pixels right of the rows beneath it.
        let mut body = v_flex()
            .py(px(2.))
            .child(self.detail_header_row(detail, &mono, cx));

        match detail.commit.as_deref() {
            Some(commit) => {
                body = body
                    .child(self.detail_message(detail, commit, cx))
                    .children(self.detail_refs(commit, &mono, cx))
                    .children(self.detail_parents(detail, commit, &mono, cx))
                    .child(self.detail_files(detail, commit, &mono, cx));
            }
            // Nothing came back. `loaded` is what tells "still reading" apart
            // from "git has no such commit here" — without it a bad oid would
            // read as a spinner that never stops.
            None => {
                body = body.child(
                    div()
                        .px(px(CONTENT_INSET))
                        .py(px(4.))
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(if detail.loaded {
                            t(L10nKey::ScmCommitNotFound)
                        } else {
                            t(L10nKey::PanelLoading)
                        }),
                );
            }
        }
        Some(body.into_any_element())
    }

    /// Read the commit and its file list, once.
    ///
    /// Runs from `render`, so it has to be idempotent in the strongest sense:
    /// the panel is redrawn on every status change and a second dispatch would
    /// mean a `git show` per frame. `loading` covers the window while a read
    /// is out and `loaded` covers every frame after it lands, including the
    /// ones where it landed with nothing.
    fn load_commit_detail(&mut self, detail: &CommitDetailView, cx: &mut Context<Self>) {
        if detail.loading || detail.loaded {
            return;
        }
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, detail.repo.host) else {
            return;
        };
        if let Some(open) = self.scm.detail.as_mut() {
            open.loading = true;
        }
        let root = detail.repo.root.clone();
        let oid = detail.oid.clone();
        // A seeded view already has its metadata and only wants the files, so
        // the `show` is skipped rather than run for an answer we hold.
        let seeded = detail.commit.is_some();
        let key = (detail.repo.clone(), detail.oid.clone());
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                use tty7_core::core::git::log;
                let commit = (!seeded)
                    .then(|| log::load_commit(h, &root, &oid))
                    .flatten();
                (commit, log::commit_files(h, &root, &oid))
            },
            move |this, (commit, files), cx| {
                // The user may have gone back, or moved on to another commit,
                // while the read was out. Landing it anywhere but on the view
                // that asked would show one commit's files under another's
                // message.
                let Some(open) = this
                    .scm
                    .detail
                    .as_mut()
                    .filter(|d| (d.repo.clone(), d.oid.clone()) == key)
                else {
                    return;
                };
                open.loading = false;
                open.loaded = true;
                if let Some(commit) = commit {
                    open.commit = Some(Arc::new(commit));
                }
                open.files = Some(Arc::new(files.unwrap_or_default()));
                cx.notify();
            },
        );
    }

    /// The way back, and the object id.
    ///
    /// The back affordance belongs in `panel_title`'s trailing slot, where the
    /// diff overlay puts its own. It is here instead because the title is
    /// rendered by the panel and this function only produces the body — see
    /// the note in `render_panel_scm`. Being the first row of the body it
    /// scrolls with the content, which is the one thing lost by the move.
    fn detail_header_row(
        &self,
        detail: &CommitDetailView,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let oid = detail.oid.clone();
        h_flex()
            .items_center()
            .gap(px(4.))
            .h(px(ROW_H))
            .px(px(CONTENT_INSET - ROW_INSET))
            .child(
                h_flex()
                    .id("scm-detail-back")
                    .items_center()
                    .gap(px(2.))
                    .px(px(4.))
                    .py(px(1.))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .on_click(cx.listener(|this, _, _window, cx| this.close_commit_detail(cx)))
                    .child(
                        Icon::new(IconName::ChevronLeft)
                            .small()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().text_xs().child(t(L10nKey::ScmBackToChanges))),
            )
            .child(div().flex_1().min_w_0())
            .child(
                div()
                    .id("scm-detail-sha")
                    .flex_none()
                    .px(px(4.))
                    .py(px(1.))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .text_size(px(13.))
                    .font_family(mono.clone())
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(t(L10nKey::ScmCopyCommitSha))
                            .build(window, cx)
                    })
                    .on_click(move |_, _window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(oid.clone()));
                    })
                    .child(short_oid(&detail.oid).to_string()),
            )
            .into_any_element()
    }

    /// Subject, byline, body.
    fn detail_message(
        &self,
        detail: &CommitDetailView,
        commit: &Commit,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = commit.body.trim();
        let lines = body.lines().count();
        let folded = !detail.body_expanded && lines > BODY_LINES;
        v_flex()
            .px(px(CONTENT_INSET))
            .pb(px(4.))
            .gap(px(3.))
            .child(
                // Wrapping, not truncating: this view exists because the
                // graph row could only show the first 26 characters.
                div()
                    .text_size(px(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .line_clamp(SUBJECT_LINES)
                    .child(SharedString::from(commit.summary.clone())),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(byline(commit, now_unix())),
            )
            .when(!body.is_empty(), |this| {
                this.child(
                    div()
                        .pt(px(2.))
                        .text_size(px(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .when(folded, |d| d.line_clamp(BODY_LINES))
                        .child(SharedString::from(body.to_string())),
                )
                .when(lines > BODY_LINES, |this| {
                    this.child(
                        div()
                            .id("scm-detail-body-fold")
                            .w_full()
                            .py(px(1.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(cx.theme().info)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                if let Some(open) = this.scm.detail.as_mut() {
                                    open.body_expanded = !open.body_expanded;
                                    cx.notify();
                                }
                            }))
                            .child(t(if folded {
                                L10nKey::ScmShowMore
                            } else {
                                L10nKey::ScmShowLess
                            })),
                    )
                })
            })
            .into_any_element()
    }

    /// Every ref pointing here, wrapped over as many lines as it takes.
    ///
    /// The graph row shows one chip and a `+N`; there is no reason to hide any
    /// of them once there is a whole column to put them in.
    fn detail_refs(
        &self,
        commit: &Commit,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if commit.refs.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (accent, warning, fg, muted) = (
            theme.accent,
            theme.warning,
            theme.foreground,
            theme.muted_foreground,
        );
        let mut row = h_flex()
            .flex_wrap()
            .items_center()
            .gap(px(4.))
            .px(px(CONTENT_INSET))
            .pb(px(6.));
        for deco in &commit.refs {
            // The same three colours the graph's chips use: a tag is yellow
            // because a tag is yellow everywhere in git, HEAD is emphasised,
            // and everything else is quiet.
            let (bg, color) = match deco.kind {
                RefKind::Tag => (warning.opacity(0.16), warning),
                _ if deco.is_head => (accent.opacity(0.28), fg),
                _ => (accent, muted),
            };
            row = row.child(info_chip(&deco.short, bg, color, mono));
        }
        Some(row.into_any_element())
    }

    /// The parents, as links. Following one is the only way to walk history
    /// backwards from a commit the graph's window does not reach.
    fn detail_parents(
        &self,
        detail: &CommitDetailView,
        commit: &Commit,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if commit.parents.is_empty() {
            return None;
        }
        let mut row = h_flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .px(px(CONTENT_INSET))
            .pb(px(4.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(t(L10nKey::ScmCommitParents)),
            );
        for parent in &commit.parents {
            let repo = detail.repo.clone();
            let oid = parent.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("scm-detail-parent-{parent}")))
                    .px(px(3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .text_size(px(11.))
                    .font_family(mono.clone())
                    .text_color(cx.theme().info)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        // No seed: a parent is by definition one step past
                        // whatever the caller had in hand.
                        this.open_commit_detail(repo.clone(), oid.clone(), None, cx);
                    }))
                    .child(short_oid(parent).to_string()),
            );
        }
        Some(row.into_any_element())
    }

    fn detail_files(
        &self,
        detail: &CommitDetailView,
        commit: &Commit,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let files = detail.files.clone().unwrap_or_default();
        let mut list = v_flex().child(self.panel_subtitle(
            &t_plural(L10nKey::ScmFilesChanged, files.len(), &[]),
            true,
            None,
            cx,
        ));
        if detail.files.is_none() {
            return list
                .child(self.detail_note(t(L10nKey::PanelLoading).to_string(), cx))
                .into_any_element();
        }
        // The label rides along on the source so the overlay's header can say
        // which commit it is showing, and it is deliberately not part of that
        // source's identity — the same commit opened from here and from
        // anywhere else has to stay one overlay.
        let source = DiffSource::Commit {
            rev: detail.oid.clone(),
            label: Some(CommitLabel {
                subject: commit.summary.clone(),
                author: commit.author.name.clone(),
                at: commit.author.at.unix,
            }),
        };
        // The rows sit in the working tree's own column: laid out one
        // `ROW_INSET` short of `CONTENT_INSET` and padding themselves back
        // out, so a hovered row's background is wider than its text.
        let mut rows = v_flex().px(px(CONTENT_INSET - ROW_INSET));
        for file in files.iter() {
            rows = rows.child(self.detail_file_row(detail, &source, file, mono, cx));
        }
        list.child(rows).into_any_element()
    }

    /// The working tree's file row, minus the hover buttons.
    ///
    /// A copy of `scm_file_row`, which is the wrong way round and known to be:
    /// the two have to stay pixel-identical and nothing here enforces that.
    /// They differ only in what they are built from — a `StatusEntry` against
    /// a [`CommitFile`] — and in the buttons, so the shared version is a
    /// function over `(letter, deco, path)` plus an optional trailing element.
    fn detail_file_row(
        &self,
        detail: &CommitDetailView,
        source: &DiffSource,
        file: &CommitFile,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let deco = crate::ui::diff_overlay::deco_status(file.status);
        let (name, dir) = split_display_path(&file.path);
        let selected = self.diff_overlay_focus(detail.repo.host, &detail.repo.root)
            == Some(file.path.as_str());

        h_flex()
            .id(SharedString::from(format!("scm-detail-file-{}", file.path)))
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
                let repo = detail.repo.clone();
                let source = source.clone();
                let path = file.path.clone();
                cx.listener(move |this, _, window, cx| {
                    // 260px cannot render a patch, so the file level is the
                    // full-screen overlay's job — the same one the working
                    // tree's rows open, pointed at a commit instead.
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
            .child(git_badge(status_glyph(deco), status_color(deco, cx), mono))
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

    fn detail_note(&self, text: String, cx: &mut Context<Self>) -> AnyElement {
        div()
            .px(px(CONTENT_INSET))
            .py(px(3.))
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground.opacity(0.75))
            .child(text)
            .into_any_element()
    }
}

/// `Ada Lovelace · 2h`. Author, not committer: a rebase rewrites the second
/// one, and "who wrote this" is the question a reader is asking.
pub(crate) fn byline(commit: &Commit, now: i64) -> String {
    let when = (commit.author.at.unix > 0).then(|| relative_time(now, commit.author.at.unix));
    match (commit.author.name.trim(), when) {
        ("", Some(when)) => when,
        (name, Some(when)) => format!("{name} · {when}"),
        (name, None) => name.to_string(),
    }
}

/// Seven, which is what git itself prints and what the graph's rows use.
pub(crate) fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tty7_core::core::git::log::{OffsetTs, Signature};

    fn commit(name: &str, at: i64) -> Commit {
        Commit {
            oid: "3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a".into(),
            parents: Default::default(),
            author: Signature {
                name: name.into(),
                email: "ada@example.com".into(),
                at: OffsetTs {
                    unix: at,
                    offset_minutes: 0,
                },
            },
            committer: Signature {
                name: "Grace".into(),
                email: "grace@example.com".into(),
                at: OffsetTs {
                    unix: at,
                    offset_minutes: 0,
                },
            },
            summary: "s".into(),
            body: String::new(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn the_byline_drops_the_separator_along_with_the_half_it_joined() {
        let now = 1_786_255_391 + 7200;
        assert_eq!(byline(&commit("Ada", 1_786_255_391), now), "Ada · 2h");
        assert_eq!(
            byline(&commit("", 1_786_255_391), now),
            "2h",
            "an unattributed commit is not `· 2h`"
        );
        assert_eq!(
            byline(&commit("Ada", 0), now),
            "Ada",
            "and a date that would not parse is not `Ada · 56y`"
        );
    }

    #[test]
    fn a_short_oid_is_the_seven_characters_git_itself_prints() {
        assert_eq!(
            short_oid("3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a"),
            "3f2a1b9"
        );
        assert_eq!(short_oid("abc"), "abc", "a truncated oid is not padded");
        assert_eq!(short_oid(""), "");
    }
}

/// The detail view against a real repository, drawn in a real window.
///
/// Construction alone would prove very little: everything that can go wrong
/// here — a missing global, a theme token, a slice through the middle of a
/// character — goes wrong during layout and paint, so these arm the render
/// probe and insist something was actually drawn.
#[cfg(all(test, unix))]
mod detail_gpui_tests {
    use super::*;
    use crate::daemon::protocol::DaemonMsg;
    use crate::ui::app::{render_probe, test_window};
    use crate::ui::host_ops::HostId;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::path::{Path, PathBuf};
    use tty7_core::core::config::RightPanelTab;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-detail-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Two commits: a root, then one that renames a file, adds a path with a
    /// space in it and writes a body long enough to fold.
    fn two_commit_repo(name: &str) -> PathBuf {
        let root = scratch(name);
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "ada@example.com"]);
        git(&root, &["config", "user.name", "Ada"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-qm", "root commit"]);
        std::fs::rename(root.join("a.txt"), root.join("renamed.txt")).unwrap();
        std::fs::write(root.join("with space.txt"), "two\n").unwrap();
        std::fs::write(root.join("中文名.txt"), "three\n").unwrap();
        git(&root, &["add", "-A"]);
        git(
            &root,
            &[
                "commit",
                "-qm",
                "feat(detail): a subject long enough that the graph row could never have shown it",
                "-m",
                "one\ntwo\nthree\nfour\nfive\nsix",
            ],
        );
        root
    }

    fn panel_on(
        cx: &mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Tty7App>,
        VisualTestContext,
        std::os::unix::net::UnixStream,
    ) {
        let (app, mut vcx, mut pane) = test_window::harness_with_pane(cx);
        DaemonMsg::Cwd(root.to_path_buf())
            .encode(&mut pane)
            .expect("the pane's socket takes the cwd");
        app.update_in(&mut vcx, |app, _, cx| {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Scm;
            cx.notify();
        });
        let want = root.to_path_buf();
        settle(&app, &mut vcx, move |app, _| {
            app.scm.repo.as_ref().is_some_and(|r| r.root == want)
        });
        (app, vcx, pane)
    }

    /// Pump frames until the panel has done what it was asked. The panel only
    /// starts a read from `render`, so nothing here can be awaited directly.
    fn settle(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
        done: impl Fn(&Tty7App, &gpui::App) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            app.update_in(vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
            if app.update_in(vcx, |app, _, cx| done(app, cx)) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the panel never settled"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn paths(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Vec<String> {
        app.update_in(vcx, |app, _, _| {
            app.scm
                .detail
                .as_ref()
                .and_then(|d| d.files.clone())
                .map(|files| files.iter().map(|f| f.path.clone()).collect())
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    fn a_commit_detail_reads_its_own_files_and_draws_them(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("draws");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);

        let repo = app.update_in(&mut vcx, |app, _, _| app.scm.repo.clone().unwrap());
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), head.clone(), None, cx)
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });

        // Nothing was seeded, so the metadata came from `git show`.
        app.update_in(&mut vcx, |app, _, _| {
            let detail = app.scm.detail.as_ref().expect("the detail is open");
            let commit = detail.commit.as_ref().expect("git resolved the commit");
            assert_eq!(commit.oid, head);
            assert!(commit.summary.starts_with("feat(detail):"));
            assert_eq!(commit.parents.len(), 1);
            assert_eq!(commit.body.lines().count(), 6, "long enough to fold");
        });
        let mut listed = paths(&app, &mut vcx);
        listed.sort();
        assert_eq!(
            listed,
            ["renamed.txt", "with space.txt", "中文名.txt"],
            "the two -z streams joined into one list"
        );

        // A real frame, so layout and paint run over every row above.
        render_probe::arm(10_000);
        app.update_in(&mut vcx, |_, _, cx| cx.notify());
        vcx.background_executor.run_until_parked();
        assert!(
            render_probe::draws() > 0,
            "nothing was drawn, so nothing was proved"
        );

        // Expanding the body is another branch of the same element.
        app.update_in(&mut vcx, |app, _, cx| {
            app.scm.detail.as_mut().unwrap().body_expanded = true;
            cx.notify();
        });
        render_probe::arm(10_000);
        app.update_in(&mut vcx, |_, _, cx| cx.notify());
        vcx.background_executor.run_until_parked();
        assert!(render_probe::draws() > 0);

        // The read runs from `render`, which is the shape that has spun this
        // panel before: a dispatch that did not record itself would ask git
        // for the same commit again on the frame its own answer caused.
        assert_eq!(draws_while_idle(&mut vcx), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Copied from `panel.rs`'s own idle tests: arm the probe, let every timer
    /// the panel owns fire, and count the frames nobody asked for.
    fn draws_while_idle(vcx: &mut VisualTestContext) -> u64 {
        render_probe::arm(200);
        vcx.background_executor.run_until_parked();
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        render_probe::arm(200);
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();
        render_probe::draws()
    }

    #[gpui::test]
    fn following_a_parent_swaps_the_commit_and_going_back_clears_it(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("parent");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let parent = git(&root, &["rev-parse", "HEAD^"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);

        let repo = app.update_in(&mut vcx, |app, _, _| app.scm.repo.clone().unwrap());
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), head.clone(), None, cx)
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });

        // What the parent link does: the same call with the other oid, and
        // nothing carried over from the commit that was on screen.
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), parent.clone(), None, cx)
        });
        app.update_in(&mut vcx, |app, _, _| {
            let detail = app.scm.detail.as_ref().unwrap();
            assert_eq!(detail.oid, parent);
            assert!(detail.commit.is_none(), "the old commit did not linger");
            assert!(!detail.loaded);
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });
        assert_eq!(
            paths(&app, &mut vcx),
            ["a.txt"],
            "a root commit's files are what it added, with no --root needed"
        );

        app.update_in(&mut vcx, |app, _, cx| app.close_commit_detail(cx));
        assert!(app.update_in(&mut vcx, |app, _, _| app.scm.detail.is_none()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_seeded_detail_only_asks_for_the_files(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("seeded");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);
        let repo = app.update_in(&mut vcx, |app, _, _| app.scm.repo.clone().unwrap());

        // What the graph hands over: a row it already holds. The subject is
        // deliberately not the real one, so a `git show` behind our back would
        // overwrite it and show up here.
        let mut seed = tty7_core::core::git::log::load_commit(
            &*tty7_core::host::local::LocalHost::new(),
            &root,
            &head,
        )
        .expect("the scratch repo answers");
        seed.summary = "what the graph already knew".into();
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), head.clone(), Some(seed), cx)
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });

        app.update_in(&mut vcx, |app, _, _| {
            let detail = app.scm.detail.as_ref().unwrap();
            assert_eq!(
                detail.commit.as_ref().unwrap().summary,
                "what the graph already knew",
                "the seed was kept, so no second read of the same commit happened"
            );
            assert_eq!(detail.files.as_ref().unwrap().len(), 3);
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A commit from a repository the panel has since walked away from is not
    /// a second-level view of anything.
    #[gpui::test]
    fn a_detail_from_another_repository_gives_the_body_back(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("elsewhere");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);

        app.update_in(&mut vcx, |app, window, cx| {
            let stranger = RepoKey {
                host: HostId::LOCAL,
                root: PathBuf::from("/no/such/tty7/repo"),
            };
            app.open_commit_detail(stranger.clone(), head.clone(), None, cx);
            let detail = app.scm.detail.clone().unwrap();
            assert!(app.render_commit_detail(&detail, window, cx).is_none());
            assert!(app.scm.detail.is_none(), "and it does not come back");
        });
        let _ = std::fs::remove_dir_all(&root);
    }
}
