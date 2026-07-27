//! Session persistence: remember the tab / split-pane layout and each
//! terminal's working directory across restarts, plus a stack of recently
//! closed tabs for "Reopen Closed Tab".
//!
//! The on-disk model mirrors the live `Pane` tree but stays purely
//! serializable (no GPUI entities, no `gpui::Axis` which isn't `Serialize`).
//! It lives at `~/.config/tty7/session.json`, alongside `config.json`.
//!
//! All IO and parsing is best-effort: a missing/corrupt file just means "no
//! session to restore", and write failures are logged rather than fatal — the
//! app must never crash or stall over session bookkeeping.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::daemon::protocol::NativeSshSpec;

/// Split orientation, mirroring `gpui::Axis` (which isn't `Serialize`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SessionAxis {
    Horizontal,
    Vertical,
}

/// A serializable mirror of one tab's `Pane` tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionPane {
    /// A single terminal, restored in `cwd` (or the default dir if `None`).
    Leaf {
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// Daemon pane id this leaf was mirroring. On restore we re-`attach` to
        /// it when the daemon still has it alive (process + scrollback intact),
        /// else fall back to spawning a fresh shell in `cwd`. `None` for sessions
        /// written by an older build (they just spawn fresh).
        #[serde(default)]
        pane_id: Option<u64>,
        /// The native-SSH spec this leaf ran, **with secrets stripped**
        /// ([`NativeSshSpec::without_secrets`]). Persisted so a *dead* native-SSH
        /// pane can be respawned (reconnected) on restore rather than falling back
        /// to a local shell — the reconnection UX itself is WS6's. A live pane
        /// reattaches for free and needs none of this. `None` for local panes and
        /// for sessions written before this field existed.
        #[serde(default)]
        ssh_spec: Option<Box<NativeSshSpec>>,
        /// The coding agent this leaf was running at save time, plus its native
        /// session id (from the agent's own `session-start` event). When the
        /// pane can't re-attach on restore, these drive the cmux-style resume:
        /// the fresh shell is handed the agent's resume command
        /// (`claude --resume <id>`, …) so the conversation continues. `None`
        /// for panes without an agent, agents without hooks, or old sessions.
        #[serde(default)]
        agent: Option<crate::core::cli_agent::CLIAgent>,
        #[serde(default)]
        agent_session_id: Option<String>,
        /// The argv the agent was launched with, as the daemon observed it —
        /// lets the resume command carry the user's launch flags
        /// (`--dangerously-skip-permissions`, …) instead of resuming bare.
        /// `None` for old sessions or when nothing was captured.
        #[serde(default)]
        agent_launch_argv: Option<Vec<String>>,
    },
    /// A split of two subtrees along `axis`, with `a` taking `ratio` of space.
    Split {
        axis: SessionAxis,
        #[serde(default = "default_ratio")]
        ratio: f32,
        a: Box<SessionPane>,
        b: Box<SessionPane>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

/// A serializable mirror of one tab: its pane tree plus an optional user-set
/// name (from "Rename Tab"). A missing `name` falls back to the title-derived
/// label at render time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    #[serde(default)]
    pub name: Option<String>,
    pub pane: SessionPane,
    /// The tab's last-known sidebar repo group (its repository home — the
    /// main checkout's root, shared by all its linked worktrees), so a
    /// restored session renders grouped immediately instead of starting flat
    /// and reshuffling as git probes land. `None` = Scratch / never resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_group: Option<std::path::PathBuf>,
}

/// One workspace's contents: the open tabs and which one was active.
///
/// This is the unit a single window displays. It used to *be* the whole file
/// (tty7 had exactly one window); it is now nested inside a [`Workspace`], and
/// [`Workspaces`] owns the file-level IO.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub active: usize,
    pub tabs: Vec<SessionTab>,
}

/// Stable identity for a workspace, minted once when it is first created and
/// carried across restarts. Windows are transient views; *this* is what the
/// workspace picker reopens and what a window handle maps back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(uuid::Uuid);

impl WorkspaceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// A stable numeric key for gpui element ids, which need something
    /// hashable and cheap rather than a freshly formatted string each frame.
    pub fn element_key(&self) -> u64 {
        self.0.as_u64_pair().0
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A persistent workspace: a named group of tabs that a window can open, close,
/// and reopen later. Closing its window is a *detach* — the panes keep running
/// in the daemon and the entry stays here with `open: false`, which is what the
/// home-page picker lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: WorkspaceId,
    /// User-set name from "Rename Workspace". `None` falls back to
    /// [`Workspace::display_name`], derived from the tabs' repo/cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub session: Session,
    /// Geometry this workspace's window last occupied, so reopening it lands
    /// where the user left it rather than at the shared default. `None` for a
    /// workspace that has never been on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<crate::core::window_state::WindowState>,
    /// Whether a window was showing this workspace at quit. Launch reopens
    /// exactly the `open` ones; the rest wait in the picker.
    #[serde(default)]
    pub open: bool,
    /// Unix seconds when this workspace was last focused, for "2 minutes ago"
    /// in the picker and for ordering it. 0 == never recorded.
    #[serde(default)]
    pub last_active: u64,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            id: WorkspaceId::new(),
            name: None,
            session: Session::default(),
            window: None,
            open: true,
            last_active: now_secs(),
        }
    }
}

impl Workspace {
    /// Wrap a bare session as a brand-new open workspace.
    pub fn from_session(session: Session) -> Self {
        Self {
            session,
            ..Self::default()
        }
    }

    /// What to show in the picker and the window title: the user-set name if
    /// any, else the repository most of its tabs live in, else the first tab's
    /// directory, else a generic fallback. Derived rather than stored so a
    /// workspace that `cd`s into a project stops being "Untitled" on its own.
    pub fn display_name(&self) -> String {
        if let Some(name) = self
            .name
            .as_ref()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
        {
            return name.to_string();
        }
        if let Some(repo) = self.dominant_repo() {
            if let Some(base) = basename(&repo) {
                return base;
            }
        }
        if let Some(cwd) = self.first_cwd() {
            if let Some(base) = basename(&cwd) {
                return base;
            }
        }
        "Untitled".to_string()
    }

    /// The repo root the most tabs belong to — the workspace's centre of
    /// gravity for naming. Ties break toward the earliest tab, matching the
    /// order the user sees in the sidebar.
    pub fn dominant_repo(&self) -> Option<PathBuf> {
        let mut counts: Vec<(PathBuf, usize)> = Vec::new();
        for group in self
            .session
            .tabs
            .iter()
            .filter_map(|t| t.sidebar_group.as_ref())
        {
            match counts.iter_mut().find(|(path, _)| path == group) {
                Some((_, n)) => *n += 1,
                None => counts.push((group.clone(), 1)),
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(path, _)| path)
    }

    /// The first saved cwd anywhere in the tab tree, used for naming and for
    /// the picker's dim subtitle line.
    pub fn first_cwd(&self) -> Option<PathBuf> {
        self.session
            .tabs
            .iter()
            .find_map(|tab| first_leaf_cwd(&tab.pane))
    }

    /// Total leaf terminals across every tab — the picker's "3 panes" count.
    pub fn pane_count(&self) -> usize {
        self.session.tabs.iter().map(|t| leaf_count(&t.pane)).sum()
    }

    /// Every daemon pane id this workspace claims, for the cross-window
    /// uniqueness check on restore (two windows attaching one pane would let
    /// the second silently steal the first's stream).
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for tab in &self.session.tabs {
            collect_pane_ids(&tab.pane, &mut out);
        }
        out
    }

    /// Stamp this workspace as just-focused.
    pub fn touch(&mut self) {
        self.last_active = now_secs();
    }
}

/// The whole `session.json`: every workspace tty7 knows about, plus which one
/// had focus at quit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workspaces {
    /// Note: deliberately *not* `#[serde(default)]` at the struct level — the
    /// presence of this key is what distinguishes a new-format file from the
    /// legacy flat `{active, tabs}` one. See [`Workspaces::decode`].
    pub workspaces: Vec<Workspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<WorkspaceId>,
}

impl Workspaces {
    /// Load every saved workspace. Returns `None` when the file is absent or
    /// unreadable (normal first run), and `None` with a warning when it fails
    /// to parse — never panics.
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        match Self::decode(&text) {
            Ok(loaded) => Some(loaded),
            Err(e) => {
                log::warn!(
                    "failed to parse session at {}: {e}; ignoring",
                    path.display()
                );
                None
            }
        }
    }

    /// Parse either format. A file written by any build with multi-window
    /// support has a `workspaces` array; anything else is a pre-multi-window
    /// `{active, tabs}` session, which migrates to a single open workspace so
    /// upgrading users keep their tabs (and their attached daemon panes).
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(crate::core::config::strip_bom(text))?;
        if value.get("workspaces").is_some() {
            return serde_json::from_value(value);
        }
        let legacy: Session = serde_json::from_value(value)?;
        Ok(Self::single(Workspace::from_session(legacy)))
    }

    /// A one-workspace set, used by the legacy migration and by first run.
    pub fn single(workspace: Workspace) -> Self {
        Self {
            active: Some(workspace.id),
            workspaces: vec![workspace],
        }
    }

    pub fn get(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn get_mut(&mut self, id: WorkspaceId) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }

    /// The workspaces to reopen at launch, in their saved order. Empty when
    /// the user quit with every window closed — launch then shows one window on
    /// the picker rather than guessing.
    pub fn open_workspaces(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.iter().filter(|w| w.open)
    }

    /// Closed workspaces for the home-page picker, most recently active first.
    pub fn closed_workspaces(&self) -> Vec<&Workspace> {
        let mut closed: Vec<&Workspace> = self.workspaces.iter().filter(|w| !w.open).collect();
        closed.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        closed
    }

    /// Drop pane ids that appear in more than one workspace, keeping the claim
    /// of whichever workspace was active most recently. A duplicate would have
    /// two windows attach the same daemon pane, and the daemon's single
    /// subscriber means the loser's terminal goes silently dead — so this runs
    /// on every load, before any window is built.
    ///
    /// Returns the number of claims dropped (0 in the healthy case).
    pub fn dedupe_pane_ids(&mut self) -> usize {
        let mut order: Vec<(usize, u64)> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| (i, w.last_active))
            .collect();
        // Most recently active first: it keeps its claim, earlier ones yield.
        order.sort_by(|a, b| b.1.cmp(&a.1));

        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut dropped = 0;
        for (index, _) in order {
            let workspace = &mut self.workspaces[index];
            for tab in &mut workspace.session.tabs {
                dropped += drop_duplicate_pane_ids(&mut tab.pane, &mut seen);
            }
        }
        dropped
    }

    /// Persist as JSON, creating the parent directory if needed. Any
    /// IO/serialization error is logged and swallowed — the app must never
    /// crash or stall over session bookkeeping.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("failed to create session dir {}: {e}", parent.display());
                return;
            }
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize session: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write session to {}: {e}", path.display());
        }
    }

    /// `~/.config/tty7/session.json`, alongside `config.json`.
    fn path() -> Option<PathBuf> {
        crate::core::config::config_path("session.json")
    }
}

/// App-level owner of `session.json`, and the single writer to it.
///
/// Windows never touch the file themselves. Each one pushes *its* workspace's
/// state in and the store persists the merged whole — without that, two windows
/// doing read-modify-write on the shared file would have the last writer
/// clobber the other's tabs. It also means a window that is closing can record
/// its final state after its own entity is already being torn down.
pub struct WorkspaceStore {
    workspaces: Workspaces,
}

impl gpui::Global for WorkspaceStore {}

impl WorkspaceStore {
    /// Read `session.json` (migrating a legacy flat session), drop any
    /// duplicate pane claims, and install the result as the app global. Call
    /// once, before the first window is built.
    pub fn init(cx: &mut gpui::App) {
        let mut workspaces = Workspaces::load().unwrap_or_default();
        let dropped = workspaces.dedupe_pane_ids();
        if dropped > 0 {
            log::warn!(
                "session.json claimed {dropped} pane(s) from more than one workspace; \
                 the stale claims will spawn fresh shells instead"
            );
        }
        cx.set_global(Self { workspaces });
    }

    /// Every known workspace. Read-only — mutations go through the helpers so
    /// the file stays in step.
    ///
    /// Reads as empty when the store was never installed. That is the headless
    /// test harness, which builds windows directly rather than through
    /// `ui::windows::open`; "no saved workspaces" is the correct reading there,
    /// and it keeps a missing global from panicking a render.
    pub fn all(cx: &gpui::App) -> &Workspaces {
        static EMPTY: std::sync::OnceLock<Workspaces> = std::sync::OnceLock::new();
        match cx.try_global::<Self>() {
            Some(store) => &store.workspaces,
            None => EMPTY.get_or_init(Workspaces::default),
        }
    }

    /// The store, or `None` when it was never installed (tests). Every mutating
    /// helper goes through this so a headless window is a no-op rather than a
    /// panic — and, importantly, so tests never write to a real `session.json`.
    fn try_store(cx: &mut gpui::App) -> Option<&mut Self> {
        cx.has_global::<Self>().then(|| cx.global_mut::<Self>())
    }

    /// Take over an existing workspace to show in a window, or mint a fresh one
    /// when `id` is `None` / no longer on file (the "New Workspace" path). Marks it
    /// open and returns its id plus the tabs the window should rebuild.
    pub fn claim(cx: &mut gpui::App, id: Option<WorkspaceId>) -> (WorkspaceId, Session) {
        let Some(store) = Self::try_store(cx) else {
            // No store (tests): hand back a detached identity so the window
            // still builds, but nothing is persisted.
            return (WorkspaceId::new(), Session::default());
        };
        let id = id.filter(|id| store.workspaces.get(*id).is_some());
        let workspace = match id {
            Some(id) => store.workspaces.get_mut(id).expect("filtered above"),
            None => {
                store.workspaces.workspaces.push(Workspace::default());
                store.workspaces.workspaces.last_mut().expect("just pushed")
            }
        };
        workspace.open = true;
        workspace.touch();
        let claimed = (workspace.id, workspace.session.clone());
        store.workspaces.active = Some(claimed.0);
        store.workspaces.save();
        claimed
    }

    /// Record a window's current tabs (and geometry, when known) and persist.
    /// Called on every structural change, exactly where `Session::save` used to be.
    pub fn record(
        cx: &mut gpui::App,
        id: WorkspaceId,
        session: Session,
        window: Option<crate::core::window_state::WindowState>,
    ) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        let Some(workspace) = store.workspaces.get_mut(id) else {
            // The workspace was closed out from under us (its window is
            // tearing down); nothing to record.
            return;
        };
        workspace.session = session;
        if let Some(window) = window {
            workspace.window = Some(window);
        }
        store.workspaces.save();
    }

    /// Mark the focused workspace, so the next launch restores focus to the
    /// window the user was actually in.
    pub fn focus(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.touch();
        }
        store.workspaces.active = Some(id);
        store.workspaces.save();
    }

    /// Set (or clear, with `None`) a workspace's user-chosen name. Clearing
    /// falls back to the derived repo/cwd name — see [`Workspace::display_name`].
    pub fn rename(cx: &mut gpui::App, id: WorkspaceId, name: Option<String>) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.name = name;
        }
        store.workspaces.save();
    }

    /// Detach a workspace: its window is gone, but the panes keep running in
    /// the daemon and the entry stays for the picker to reopen.
    pub fn close_window(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.open = false;
            workspace.touch();
        }
        store.workspaces.save();
    }

    /// Forget a workspace entirely — the explicit "Close Workspace" action.
    /// The caller is responsible for killing its daemon panes first; this only
    /// drops the bookkeeping.
    pub fn remove(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        store.workspaces.workspaces.retain(|w| w.id != id);
        if store.workspaces.active == Some(id) {
            store.workspaces.active = None;
        }
        store.workspaces.save();
    }
}

/// Seconds since the Unix epoch, or 0 if the clock is before it (which only a
/// badly misconfigured machine reports — "never active" is a fine reading).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Last path component as a display string, skipping a bare `/` or a path that
/// ends in `..`.
fn basename(path: &std::path::Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn first_leaf_cwd(pane: &SessionPane) -> Option<PathBuf> {
    match pane {
        SessionPane::Leaf { cwd, .. } => cwd.clone(),
        SessionPane::Split { a, b, .. } => first_leaf_cwd(a).or_else(|| first_leaf_cwd(b)),
    }
}

fn leaf_count(pane: &SessionPane) -> usize {
    match pane {
        SessionPane::Leaf { .. } => 1,
        SessionPane::Split { a, b, .. } => leaf_count(a) + leaf_count(b),
    }
}

fn collect_pane_ids(pane: &SessionPane, out: &mut Vec<u64>) {
    match pane {
        SessionPane::Leaf { pane_id, .. } => out.extend(pane_id),
        SessionPane::Split { a, b, .. } => {
            collect_pane_ids(a, out);
            collect_pane_ids(b, out);
        }
    }
}

/// Blank any `pane_id` already claimed by an earlier-visited workspace. A
/// blanked leaf still restores — it just spawns a fresh shell in its saved cwd,
/// the same path a session from before the daemon existed takes.
fn drop_duplicate_pane_ids(
    pane: &mut SessionPane,
    seen: &mut std::collections::HashSet<u64>,
) -> usize {
    match pane {
        SessionPane::Leaf { pane_id, .. } => match *pane_id {
            Some(id) if !seen.insert(id) => {
                log::warn!("workspace claims pane {id} twice; dropping the duplicate claim");
                *pane_id = None;
                1
            }
            _ => 0,
        },
        SessionPane::Split { a, b, .. } => {
            drop_duplicate_pane_ids(a, seen) + drop_duplicate_pane_ids(b, seen)
        }
    }
}

/// Helpers for every test that touches the on-disk `session.json`. The
/// config-dir pin is process-wide (`set_config_dir` is first-call-wins), so
/// the file is process-wide too — any test that reads or writes it must hold
/// [`lock_session_file`] across the whole read/write sequence, or parallel
/// tests clobber each other's session.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static SESSION_FILE: Mutex<()> = Mutex::new(());

    /// Serialize access to the shared `session.json`.
    pub(crate) fn lock_session_file() -> MutexGuard<'static, ()> {
        // A poisoned lock just means another test failed mid-sequence; every
        // holder rewrites the file from scratch, so the state is still sound.
        SESSION_FILE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pin the process config dir at a shared temp location so `save`/`load`
    /// (which resolve `session.json` under it) never touch the real `~/.config`.
    /// `set_config_dir` is first-call-wins; every caller computes the same path.
    pub(crate) fn pin_config_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir.clone());
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{lock_session_file, pin_config_dir};
    use super::*;

    #[test]
    fn session_json_round_trips_nested_tree() {
        let session = Session {
            active: 1,
            tabs: vec![
                SessionTab {
                    name: Some("build".into()),
                    sidebar_group: None,
                    pane: SessionPane::Leaf {
                        cwd: Some(PathBuf::from("/work")),
                        pane_id: Some(7),
                        ssh_spec: None,
                        agent: None,
                        agent_session_id: None,
                        agent_launch_argv: None,
                    },
                },
                SessionTab {
                    name: None,
                    sidebar_group: None,
                    pane: SessionPane::Split {
                        axis: SessionAxis::Vertical,
                        ratio: 0.3,
                        a: Box::new(SessionPane::Leaf {
                            cwd: None,
                            pane_id: None,
                            ssh_spec: None,
                            agent: None,
                            agent_session_id: None,
                            agent_launch_argv: None,
                        }),
                        b: Box::new(SessionPane::Leaf {
                            cwd: Some(PathBuf::from("/tmp")),
                            pane_id: Some(9),
                            ssh_spec: None,
                            agent: None,
                            agent_session_id: None,
                            agent_launch_argv: None,
                        }),
                    },
                },
            ],
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active, 1);
        assert_eq!(back.tabs.len(), 2);
        assert!(matches!(
            back.tabs[0].pane,
            SessionPane::Leaf {
                pane_id: Some(7),
                ..
            }
        ));
        match &back.tabs[1].pane {
            SessionPane::Split { ratio, .. } => assert!((ratio - 0.3).abs() < 1e-6),
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn leaf_agent_resume_fields_round_trip_and_default() {
        // Round trip: the agent + native session id survive serialization.
        let leaf = SessionPane::Leaf {
            cwd: None,
            pane_id: None,
            ssh_spec: None,
            agent: Some(crate::core::cli_agent::CLIAgent::Claude),
            agent_session_id: Some("abc-123".into()),
            agent_launch_argv: Some(vec![
                "claude".into(),
                "--dangerously-skip-permissions".into(),
            ]),
        };
        let back: SessionPane =
            serde_json::from_str(&serde_json::to_string(&leaf).unwrap()).unwrap();
        match back {
            SessionPane::Leaf {
                agent,
                agent_session_id,
                agent_launch_argv,
                ..
            } => {
                assert_eq!(agent, Some(crate::core::cli_agent::CLIAgent::Claude));
                assert_eq!(agent_session_id.as_deref(), Some("abc-123"));
                assert_eq!(
                    agent_launch_argv.as_deref(),
                    Some(
                        &[
                            "claude".to_string(),
                            "--dangerously-skip-permissions".to_string()
                        ][..]
                    )
                );
            }
            _ => panic!("expected leaf"),
        }
        // A session written before these fields existed decodes with `None`s.
        let old: SessionPane =
            serde_json::from_str(r#"{"Leaf":{"cwd":"/x","pane_id":3}}"#).unwrap();
        assert!(matches!(
            old,
            SessionPane::Leaf {
                agent: None,
                agent_session_id: None,
                agent_launch_argv: None,
                ..
            }
        ));
    }

    #[test]
    fn a_utf8_bom_does_not_discard_the_session() {
        // `Session::load` treats a parse error as "no session", so a BOM on a
        // hand-edited `session.json` doesn't warn — it drops every workspace
        // and opens on the home page as if nothing had been saved.
        // Legacy `{active, tabs}` shape, so this also covers the migration path.
        let decoded = Workspaces::decode(
            "\u{FEFF}{\"active\": 0, \"tabs\": [{\"pane\": {\"Leaf\": {\"cwd\": \"/work\"}}}]}",
        )
        .expect("a BOM'd session still decodes");
        let tabs = &decoded
            .workspaces
            .first()
            .expect("migrated workspace")
            .session
            .tabs;
        assert_eq!(tabs.len(), 1);
    }

    #[test]
    fn session_defaults_fill_missing_fields() {
        // An empty object → default (active 0, no tabs).
        let s: Session = serde_json::from_str("{}").unwrap();
        assert_eq!(s.active, 0);
        assert!(s.tabs.is_empty());

        // A split without a ratio falls back to the 0.5 default, and a leaf
        // without cwd/pane_id decodes with `None`s.
        let pane: SessionPane = serde_json::from_str(
            r#"{"Split":{"axis":"Horizontal","a":{"Leaf":{}},"b":{"Leaf":{}}}}"#,
        )
        .unwrap();
        match pane {
            SessionPane::Split { ratio, .. } => assert_eq!(ratio, 0.5),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn save_then_load_recovers_the_session() {
        let _file = lock_session_file();
        pin_config_dir();
        let session = Session {
            active: 0,
            tabs: vec![SessionTab {
                name: Some("main".into()),
                sidebar_group: None,
                pane: SessionPane::Leaf {
                    cwd: Some(PathBuf::from("/home/u")),
                    pane_id: Some(1),
                    ssh_spec: None,
                    agent: None,
                    agent_session_id: None,
                    agent_launch_argv: None,
                },
            }],
        };
        Workspaces::single(Workspace::from_session(session)).save();
        let loaded = Workspaces::load().expect("a saved session should load back");
        let only = &loaded.workspaces[0];
        assert_eq!(only.session.tabs.len(), 1);
        assert_eq!(only.session.tabs[0].name.as_deref(), Some("main"));
        assert_eq!(loaded.active, Some(only.id));
    }

    // ── Workspace layer ─────────────────────────────────────────────────────

    /// Build a leaf with the given cwd + pane id; the agent/ssh fields are
    /// irrelevant to every workspace-layer test.
    fn leaf(cwd: Option<&str>, pane_id: Option<u64>) -> SessionPane {
        SessionPane::Leaf {
            cwd: cwd.map(PathBuf::from),
            pane_id,
            ssh_spec: None,
            agent: None,
            agent_session_id: None,
            agent_launch_argv: None,
        }
    }

    fn tab(pane: SessionPane, group: Option<&str>) -> SessionTab {
        SessionTab {
            name: None,
            sidebar_group: group.map(PathBuf::from),
            pane,
        }
    }

    fn workspace(tabs: Vec<SessionTab>) -> Workspace {
        Workspace::from_session(Session { active: 0, tabs })
    }

    #[test]
    fn legacy_flat_session_migrates_to_one_open_workspace() {
        // Exactly the shape every pre-multi-window build wrote.
        let legacy = r#"{"active":1,"tabs":[
            {"name":"build","pane":{"Leaf":{"cwd":"/work","pane_id":7}}},
            {"name":null,"pane":{"Leaf":{"cwd":"/tmp","pane_id":9}}}
        ]}"#;
        let loaded = Workspaces::decode(legacy).expect("legacy session should migrate");
        assert_eq!(loaded.workspaces.len(), 1);
        let only = &loaded.workspaces[0];
        // The tabs — and crucially the pane ids, which are live daemon panes —
        // survive the upgrade, so an updating user doesn't lose their shells.
        assert_eq!(only.session.active, 1);
        assert_eq!(only.session.tabs.len(), 2);
        assert_eq!(only.pane_ids(), vec![7, 9]);
        // It reopens on the next launch, matching pre-upgrade behavior.
        assert!(only.open);
        assert_eq!(loaded.active, Some(only.id));
    }

    #[test]
    fn empty_and_absent_shapes_decode_without_losing_data() {
        // `{}` is the home-page state an older build wrote: zero tabs, still valid.
        let empty = Workspaces::decode("{}").expect("empty object decodes");
        assert_eq!(empty.workspaces.len(), 1);
        assert!(empty.workspaces[0].session.tabs.is_empty());
        // A new-format file with no workspaces at all stays empty rather than
        // being mistaken for a legacy session and gaining a phantom entry.
        let none = Workspaces::decode(r#"{"workspaces":[]}"#).expect("new format decodes");
        assert!(none.workspaces.is_empty());
    }

    #[test]
    fn new_format_round_trips_through_json() {
        let mut ws = workspace(vec![tab(leaf(Some("/work"), Some(3)), Some("/work"))]);
        ws.name = Some("api".into());
        ws.open = false;
        ws.last_active = 1_700_000_000;
        let id = ws.id;
        let all = Workspaces {
            active: Some(id),
            workspaces: vec![ws],
        };
        let back = Workspaces::decode(&serde_json::to_string(&all).unwrap()).unwrap();
        let only = &back.workspaces[0];
        assert_eq!(only.id, id, "workspace identity must survive a restart");
        assert_eq!(only.name.as_deref(), Some("api"));
        assert!(!only.open);
        assert_eq!(only.last_active, 1_700_000_000);
        assert_eq!(back.active, Some(id));
    }

    #[test]
    fn display_name_prefers_user_name_then_repo_then_cwd() {
        // No name, no repo group: fall back to the first leaf's directory.
        let ws = workspace(vec![tab(leaf(Some("/home/u/scratch"), None), None)]);
        assert_eq!(ws.display_name(), "scratch");

        // A repo group wins over the cwd — it's the workspace's real subject.
        let ws = workspace(vec![tab(
            leaf(Some("/repo/tty7/src"), None),
            Some("/repo/tty7"),
        )]);
        assert_eq!(ws.display_name(), "tty7");

        // The majority repo wins when tabs straddle two checkouts.
        let ws = workspace(vec![
            tab(leaf(None, None), Some("/repo/other")),
            tab(leaf(None, None), Some("/repo/tty7")),
            tab(leaf(None, None), Some("/repo/tty7")),
        ]);
        assert_eq!(ws.display_name(), "tty7");

        // An explicit name beats everything derived.
        let mut ws = workspace(vec![tab(
            leaf(Some("/repo/tty7"), None),
            Some("/repo/tty7"),
        )]);
        ws.name = Some("  Release prep  ".into());
        assert_eq!(ws.display_name(), "Release prep");

        // Nothing to go on at all.
        assert_eq!(workspace(vec![]).display_name(), "Untitled");
        // A whitespace-only name is treated as unset rather than rendering blank.
        let mut ws = workspace(vec![tab(leaf(Some("/x/proj"), None), None)]);
        ws.name = Some("   ".into());
        assert_eq!(ws.display_name(), "proj");
    }

    #[test]
    fn pane_and_tab_counts_walk_the_split_tree() {
        let ws = workspace(vec![
            tab(leaf(Some("/a"), Some(1)), None),
            tab(
                SessionPane::Split {
                    axis: SessionAxis::Vertical,
                    ratio: 0.5,
                    a: Box::new(leaf(Some("/b"), Some(2))),
                    b: Box::new(leaf(None, Some(3))),
                },
                None,
            ),
        ]);
        assert_eq!(ws.pane_count(), 3);
        assert_eq!(ws.pane_ids(), vec![1, 2, 3]);
        assert_eq!(ws.first_cwd(), Some(PathBuf::from("/a")));
    }

    #[test]
    fn dedupe_pane_ids_keeps_the_most_recently_active_claim() {
        // Two workspaces both claim pane 5 — the crash/hand-edit case. The
        // stale one must yield, or its window silently steals the live one's
        // stream when both attach (the daemon has a single subscriber).
        let mut stale = workspace(vec![tab(leaf(Some("/old"), Some(5)), None)]);
        stale.last_active = 100;
        let mut fresh = workspace(vec![tab(leaf(Some("/new"), Some(5)), None)]);
        fresh.last_active = 200;
        let (stale_id, fresh_id) = (stale.id, fresh.id);

        let mut all = Workspaces {
            active: Some(fresh_id),
            workspaces: vec![stale, fresh],
        };
        assert_eq!(all.dedupe_pane_ids(), 1);

        // The recent one keeps pane 5; the stale one drops to a fresh spawn in
        // its saved cwd (cwd is preserved — only the id is cleared).
        assert_eq!(all.get(fresh_id).unwrap().pane_ids(), vec![5]);
        assert!(all.get(stale_id).unwrap().pane_ids().is_empty());
        assert_eq!(
            all.get(stale_id).unwrap().first_cwd(),
            Some(PathBuf::from("/old"))
        );
    }

    #[test]
    fn dedupe_pane_ids_is_a_noop_on_healthy_sessions() {
        let mut all = Workspaces {
            active: None,
            workspaces: vec![
                workspace(vec![tab(leaf(Some("/a"), Some(1)), None)]),
                workspace(vec![tab(leaf(Some("/b"), Some(2)), None)]),
            ],
        };
        assert_eq!(all.dedupe_pane_ids(), 0);
        assert_eq!(all.workspaces[0].pane_ids(), vec![1]);
        assert_eq!(all.workspaces[1].pane_ids(), vec![2]);
    }

    #[test]
    fn dedupe_pane_ids_catches_a_duplicate_within_one_workspace() {
        // Same guarantee inside a single workspace: a split that somehow ended
        // up with the same pane in both halves would deadlock the same way.
        let mut all = Workspaces {
            active: None,
            workspaces: vec![workspace(vec![
                tab(leaf(Some("/a"), Some(1)), None),
                tab(leaf(Some("/b"), Some(1)), None),
            ])],
        };
        assert_eq!(all.dedupe_pane_ids(), 1);
        assert_eq!(all.workspaces[0].pane_ids(), vec![1]);
    }

    #[test]
    fn open_and_closed_partition_by_flag_and_recency() {
        let mut open_one = workspace(vec![]);
        open_one.open = true;
        let mut older = workspace(vec![]);
        older.open = false;
        older.last_active = 100;
        let mut newer = workspace(vec![]);
        newer.open = false;
        newer.last_active = 300;
        let (open_id, older_id, newer_id) = (open_one.id, older.id, newer.id);

        let all = Workspaces {
            active: None,
            workspaces: vec![open_one, older, newer],
        };
        assert_eq!(
            all.open_workspaces().map(|w| w.id).collect::<Vec<_>>(),
            vec![open_id]
        );
        // The picker lists most-recently-active first.
        assert_eq!(
            all.closed_workspaces()
                .iter()
                .map(|w| w.id)
                .collect::<Vec<_>>(),
            vec![newer_id, older_id]
        );
    }
}
