//! Where the source control actions and palette commands land.
//!
//! One `ScmIntent` match, so the four ways of asking for a verb — the action,
//! the key binding, the palette entry and the button on the row — cannot drift
//! into meaning different things.

use gpui::{Context, PromptLevel, Window};

use tty7_core::core::git::ops::{Destructive, GitOp, PullMode};
use tty7_core::core::git::status::HeadState;

use crate::core::config::DiffViewMode;
use crate::ui::app::Tty7App;
use crate::ui::host_registry::HostRegistry;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::scm::state::RepoKey;

/// One entry point for every source control verb.
///
/// A single enum rather than fourteen methods: the actions, the palette and
/// the row buttons all want the same behaviour, and routing them through one
/// match is what keeps the three from drifting apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ScmIntent {
    Commit,
    CommitAmend,
    /// Commit, then send it on. Two operations rather than one, so the commit
    /// still stands if the network half fails.
    CommitAndPush,
    CommitAndSync,
    StageAll,
    UnstageAll,
    DiscardAll,
    Refresh,
    Sync,
    Push,
    Pull,
    Fetch,
    CheckoutBranch,
    CreateBranch,
}

impl Tty7App {
    /// Fold the history section open or shut and remember it.
    pub(crate) fn scm_toggle_graph(&mut self, cx: &mut Context<Self>) {
        let next = !self.scm.graph.expanded;
        self.scm.graph.expanded = next;
        self.update_config(cx, |cfg| cfg.scm_graph_expanded = next);
        cx.notify();
    }

    /// Flip the diff overlay between side-by-side and unified.
    ///
    /// Global rather than per-overlay, matching `diffEditor.renderSideBySide`:
    /// someone who prefers unified prefers it for every file.
    pub(crate) fn toggle_diff_view_mode(&mut self, cx: &mut Context<Self>) {
        let next = match cx.global::<crate::core::config::Config>().diff_view {
            DiffViewMode::Split => DiffViewMode::Unified,
            DiffViewMode::Unified => DiffViewMode::Split,
        };
        self.update_config(cx, |cfg| cfg.diff_view = next);
        cx.notify();
    }

    /// Run one operation against the panel's repository, asking first when it
    /// can lose work.
    ///
    /// The gate lives here rather than in `run_git_op` because
    /// [`GitOp::destructive`] is advice about what the user stands to lose,
    /// and only a window can ask them. `window.prompt` is the project's one
    /// confirmation mechanism — deleting a file in the tree already uses it —
    /// so no modal component is introduced for this.
    pub(crate) fn scm_op(
        &mut self,
        repo: RepoKey,
        op: GitOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = HostRegistry::get(cx, repo.host) else {
            return;
        };
        let Some(loss) = op.destructive() else {
            self.run_git_op(host, repo.root, op, window, cx);
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &confirm_question(&op, loss),
            None,
            &[t(L10nKey::Cancel), confirm_verb(loss)],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(1) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                app.run_git_op(host, repo.root, op, window, cx)
            });
        })
        .detach();
    }

    pub(crate) fn run_scm_action(
        &mut self,
        intent: ScmIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.scm.active_repo().cloned() else {
            return;
        };
        match intent {
            ScmIntent::Refresh => self.scm_invalidate(&repo, cx),
            ScmIntent::StageAll => self.scm_op(repo, GitOp::StageAll, window, cx),
            ScmIntent::UnstageAll => self.scm_op(repo, GitOp::UnstageAll, window, cx),
            ScmIntent::DiscardAll => self.scm_discard_all(repo, window, cx),
            ScmIntent::Commit => {
                let amend = self.scm.amend;
                self.scm_commit(repo, amend, window, cx);
            }
            ScmIntent::CommitAmend => self.scm_commit(repo, true, window, cx),
            ScmIntent::CommitAndPush => {
                let amend = self.scm.amend;
                self.scm_commit(repo.clone(), amend, window, cx);
                self.scm_push(repo, false, window, cx);
            }
            ScmIntent::CommitAndSync => {
                let amend = self.scm.amend;
                self.scm_commit(repo.clone(), amend, window, cx);
                self.scm_sync(repo, window, cx);
            }
            ScmIntent::Sync => self.scm_sync(repo, window, cx),
            ScmIntent::Push => self.scm_push(repo, false, window, cx),
            ScmIntent::Pull => self.scm_op(
                repo,
                GitOp::Pull {
                    mode: PullMode::FfOnly,
                },
                window,
                cx,
            ),
            ScmIntent::Fetch => self.scm_op(
                repo,
                GitOp::Fetch {
                    remote: None,
                    prune: false,
                },
                window,
                cx,
            ),
            // Both open a picker rather than doing anything, so they are the
            // panel's business and not this match's.
            ScmIntent::CheckoutBranch | ScmIntent::CreateBranch => {}
        }
    }

    /// Commit whatever the message box holds.
    ///
    /// The message comes from the box when it is the one on screen and from
    /// the saved draft otherwise, so the key binding and the palette entry
    /// commit the same text the user can see.
    pub(crate) fn scm_commit(
        &mut self,
        repo: RepoKey,
        amend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(status) = crate::terminal::git_data::status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let message = self.scm_message(&repo, cx);
        let plan = crate::ui::scm::panel::commit_plan(&status, amend, &message);
        if !plan.enabled {
            gpui_component::WindowExt::push_notification(
                window,
                t(L10nKey::ScmNothingToCommit).to_string(),
                cx,
            );
            return;
        }
        let all = crate::ui::scm::panel::commit_stages_everything(&status, amend);
        // Remembered so the box can be cleared once HEAD actually moves —
        // see `scm_commit_landed`.
        self.scm.committing = Some((repo.clone(), status.head.clone(), message.clone()));
        self.scm.amend = false;
        self.scm_op(
            repo,
            GitOp::Commit {
                message,
                amend,
                signoff: false,
                no_verify: false,
                all,
            },
            window,
            cx,
        );
    }

    /// What the commit box holds for a repository, whether or not it is the
    /// one currently on screen.
    fn scm_message(&self, repo: &RepoKey, cx: &gpui::App) -> String {
        match (&self.scm.commit_input, &self.scm.commit_repo) {
            (Some(input), Some(showing)) if showing == repo => input.read(cx).value().to_string(),
            _ => self.scm.draft(repo).to_string(),
        }
    }

    /// Park everything, including the files git does not track yet.
    ///
    /// `-u`, because a stash that silently leaves new files behind is a stash
    /// that did not do what "stash all" says.
    pub(crate) fn scm_stash_all(
        &mut self,
        repo: RepoKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = match self.scm_message(&repo, cx) {
            m if m.trim().is_empty() => None,
            m => Some(m),
        };
        self.scm_op(
            repo,
            GitOp::Stash {
                message,
                include_untracked: true,
            },
            window,
            cx,
        );
    }

    /// Throw away everything: tracked edits and untracked files alike.
    ///
    /// Two operations, because git has no single command for it —
    /// `checkout --` cannot touch a file it has never heard of, and `clean`
    /// cannot touch one it has.
    fn scm_discard_all(&mut self, repo: RepoKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = crate::terminal::git_data::status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let tracked: Vec<_> = status
            .unstaged()
            .chain(status.staged())
            .filter(|e| e.path.pathspec().is_some())
            .map(|e| e.path.clone())
            .collect();
        let untracked: Vec<_> = status
            .untracked()
            .filter(|e| e.path.pathspec().is_some())
            .map(|e| e.path.clone())
            .collect();
        if !tracked.is_empty() {
            self.scm_op(
                repo.clone(),
                GitOp::DiscardWorktree { paths: tracked },
                window,
                cx,
            );
        }
        if !untracked.is_empty() {
            let directories = untracked.iter().any(|p| p.as_str().ends_with('/'));
            self.scm_op(
                repo,
                GitOp::DiscardUntracked {
                    paths: untracked,
                    directories,
                },
                window,
                cx,
            );
        }
    }

    /// Push the current branch to its upstream, or publish it if it has none.
    fn scm_push(
        &mut self,
        repo: RepoKey,
        force_with_lease: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(status) = crate::terminal::git_data::status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let HeadState::Branch { name, .. } = &status.head else {
            // A detached HEAD has no branch to push, and pushing a bare sha
            // needs a refspec the panel has no way to ask for.
            return;
        };
        let (remote, branch) = match status.upstream.as_deref().and_then(split_upstream) {
            Some((remote, branch)) => (remote.to_string(), branch.to_string()),
            None => ("origin".to_string(), name.clone()),
        };
        let set_upstream = status.upstream.is_none();
        self.scm_op(
            repo,
            GitOp::Push {
                remote,
                branch,
                set_upstream,
                force_with_lease,
            },
            window,
            cx,
        );
    }

    /// Pull then push, which is what "sync" means everywhere else.
    ///
    /// A branch with no upstream has nothing to pull, so sync is a publish.
    fn scm_sync(&mut self, repo: RepoKey, window: &mut Window, cx: &mut Context<Self>) {
        let has_upstream = crate::terminal::git_data::status_of(cx, repo.host, &repo.root)
            .is_some_and(|s| s.upstream.is_some());
        if has_upstream {
            self.scm_op(
                repo.clone(),
                GitOp::Pull {
                    mode: PullMode::FfOnly,
                },
                window,
                cx,
            );
        }
        self.scm_push(repo, false, window, cx);
    }
}

/// `origin/main` → `("origin", "main")`.
///
/// The first component is the remote: a branch name may contain slashes, a
/// remote name may not.
pub(crate) fn split_upstream(upstream: &str) -> Option<(&str, &str)> {
    let (remote, branch) = upstream.split_once('/')?;
    (!remote.is_empty() && !branch.is_empty()).then_some((remote, branch))
}

/// The question a destructive operation has to answer before it runs.
fn confirm_question(op: &GitOp, loss: Destructive) -> String {
    match loss {
        Destructive::RewritesHistory => t(L10nKey::ScmAmendConfirm).to_string(),
        // One file gets named; a whole group does not, because a list of two
        // hundred paths in a system dialog says less than the count does.
        _ => match op.paths() {
            [only] => t_fmt(L10nKey::ScmDiscardConfirm, &[("path", only.as_str())]),
            _ => t(L10nKey::ScmDiscardAllConfirm).to_string(),
        },
    }
}

fn confirm_verb(loss: Destructive) -> &'static str {
    match loss {
        Destructive::RewritesHistory => t(L10nKey::ScmAmendLastCommit),
        _ => t(L10nKey::ScmDiscard),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upstream_splits_on_its_first_slash_only() {
        assert_eq!(split_upstream("origin/main"), Some(("origin", "main")));
        // Branch names carry slashes; remote names cannot.
        assert_eq!(
            split_upstream("origin/feature/auth-retry"),
            Some(("origin", "feature/auth-retry"))
        );
        assert_eq!(split_upstream("main"), None);
        assert_eq!(split_upstream("/main"), None);
        assert_eq!(split_upstream("origin/"), None);
    }
}
