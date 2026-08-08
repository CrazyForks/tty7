//! The source control panel body.
//!
//! Lifted out of `right_panel.rs` unchanged — this is still the flat
//! `git diff HEAD` list the Changes tab always showed. The groups, the real
//! status letters and the commit box land on top of it in later steps.

use gpui::{AnyElement, Context, Window, div, prelude::*, px};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use std::path::PathBuf;
use std::sync::Arc;

use crate::terminal::git_diff::MAX_RENDERED_FILES;
use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_plural};
use crate::ui::right_panel::git_badge;

impl Tty7App {
    pub(crate) fn render_panel_scm(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let target = self
            .tabs
            .get(self.active)
            .and_then(|t| t.detail_pane(window, cx))
            .and_then(|leaf| {
                let v = leaf.read(cx);
                let cwd = v
                    .git_status_cwd()
                    .map(|p| p.to_path_buf())
                    .or_else(|| v.host_cwd())?;
                Some((v.host(cx)?, cwd))
            });

        let Some((host, cwd)) = target else {
            let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
            return self.panel_scroll(
                self.panel_empty(
                    t(L10nKey::PanelNoWorkingDirectory),
                    Some(t(L10nKey::PanelNoWorkingDirectoryHint)),
                    cx,
                ),
                title,
            );
        };
        let key = (host.id(), cwd.clone());
        if self.right_panel.diff_cwd.as_ref() != Some(&key) {
            self.right_panel.diff_cwd = Some(key);
            self.right_panel.diff = None;
            self.spawn_right_panel_diff(host.clone(), cwd.clone(), cx);
        } else if self.right_panel.diff.is_none() && self.right_panel.diff_pending.is_none() {
            self.spawn_right_panel_diff(host.clone(), cwd.clone(), cx);
        }

        let count = match &self.right_panel.diff {
            Some(Some(snap)) => {
                let n = snap.files.len() + snap.untracked_count();
                (n > 0).then(|| n.to_string())
            }
            _ => None,
        };
        let title = self.panel_title(t(L10nKey::PanelScmTitle), count, None, window, cx);
        let mono = cx.theme().mono_font_family.clone();

        let inner = match &self.right_panel.diff {
            None => self.panel_empty(t(L10nKey::PanelLoading), None, cx),
            Some(None) => self.panel_empty(
                t(L10nKey::PanelNotAGitRepo),
                Some(t(L10nKey::PanelNotAGitRepoHint)),
                cx,
            ),
            Some(Some(snap)) if snap.files.is_empty() && snap.untracked.is_empty() => self
                .panel_empty(
                    t(L10nKey::PanelNoChanges),
                    Some(t(L10nKey::PanelNoChangesHint)),
                    cx,
                ),
            Some(Some(snap)) => {
                let snap = Arc::clone(snap);
                let untracked = snap.untracked_count();
                let focused = self.diff_overlay_focus(host.id(), &cwd).map(str::to_string);
                let shown = snap.files.len().min(MAX_RENDERED_FILES);
                let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
                for file in snap.files.iter().take(shown) {
                    let path = file.path.clone();
                    let (added, removed) = (file.added, file.removed);
                    let selected = focused.as_deref() == Some(path.as_str());
                    list = list.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("panel-change-{path}")))
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(3.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .hover(|s| s.bg(gpui::rgb(sf.hover)))
                            .when(selected, |s| s.bg(gpui::rgb(sf.selected)))
                            .on_click({
                                let host_id = host.id();
                                let cwd = cwd.clone();
                                let path = path.clone();
                                cx.listener(move |this, _, window, cx| {
                                    this.toggle_diff_overlay_at(
                                        host_id,
                                        cwd.clone(),
                                        Some(path.clone()),
                                        window,
                                        cx,
                                    );
                                })
                            })
                            .child(git_badge("M", cx.theme().muted_foreground, &mono))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .font_family(mono.clone())
                                    .text_color(cx.theme().foreground)
                                    .child(path),
                            )
                            .when(added > 0, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .font_family(mono.clone())
                                        .text_color(cx.theme().success)
                                        .child(format!("+{added}")),
                                )
                            })
                            .when(removed > 0, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .font_family(mono.clone())
                                        .text_color(cx.theme().danger)
                                        .child(format!("−{removed}")),
                                )
                            }),
                    );
                }
                if snap.files.len() > shown {
                    let rest = snap.files.len() - shown;
                    list = list.child(
                        div()
                            .px(px(4.))
                            .py(px(3.))
                            .text_size(px(11.5))
                            .text_color(cx.theme().muted_foreground)
                            .child(t_plural(L10nKey::PanelMoreChangedFiles, rest, &[])),
                    );
                }
                if untracked > 0 {
                    list = list.child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(3.))
                            .child(git_badge(
                                "U",
                                cx.theme().muted_foreground.opacity(0.75),
                                &mono,
                            ))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(t_plural(L10nKey::PanelUntracked, untracked, &[])),
                            ),
                    );
                }
                list.into_any_element()
            }
        };
        self.panel_scroll(inner, title)
    }

    fn spawn_right_panel_diff(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.right_panel.diff_pending.is_some() {
            return;
        }
        self.right_panel.diff_pending = Some((host.id(), cwd.clone()));
        self.spawn_shared_diff_probe(host, cwd, cx);
    }

    pub(crate) fn right_panel_refresh_changes(&mut self, cx: &mut Context<Self>) {
        if self.right_panel.diff_pending.is_some() {
            return;
        }
        let Some((id, cwd)) = self.right_panel.diff_cwd.clone() else {
            return;
        };
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, id) else {
            return;
        };
        let Some(Some(snap)) = &self.right_panel.diff else {
            return;
        };
        let Some(status) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.status_for(id, &cwd))
        else {
            return;
        };
        let stale = status.branch != snap.branch || (status.added, status.removed) != snap.totals();
        if stale {
            self.spawn_right_panel_diff(host, cwd, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::core::config::{CoreConfig, DiffViewMode, RightPanelTab};
    use crate::ui::app::test_window::harness;
    use gpui::TestAppContext;

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
}
