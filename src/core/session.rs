//! The gpui-facing half of session persistence.
//!
//! The on-disk model — [`SessionPane`], [`SessionTab`], [`Session`],
//! [`Workspace`], [`Workspaces`] and all the `session.json` IO — lives in
//! `tty7-core`: it is pure serde, and the remote server has to read and write
//! the identical file. What is left here is [`WorkspaceStore`], which is a gpui
//! `Global` and threads every mutation through `&mut App`.

pub use tty7_core::core::session::{
    RemoteRef, RemoteTarget, Session, SessionAxis, SessionPane, SessionTab, Workspace, WorkspaceId,
    Workspaces,
};
pub use tty7_core::host::HostId;

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

    /// Install a store holding exactly `workspaces`.
    ///
    /// Tests only, and it exists because [`init`](Self::init) reads the
    /// developer's real `session.json`: a test that needs a workspace to be on
    /// file must neither depend on what happens to be there nor risk writing to
    /// it. Every mutating helper already no-ops without the global, so this is
    /// the one thing a test cannot do for itself.
    #[cfg(test)]
    pub fn install_for_test(cx: &mut gpui::App, workspaces: Workspaces) {
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
        let claimed = (workspace.id, claimable_session(workspace));
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
        record_session(workspace, session);
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

    // ----- the client / remote storage split (design §10) -------------------

    /// The machine a workspace's panes are on. `HostId::LOCAL` for a workspace
    /// this client owns, and for an id that is no longer on file — a window
    /// whose workspace vanished is showing nothing, and "nothing" is here.
    pub fn host_of(cx: &gpui::App, id: WorkspaceId) -> HostId {
        host_for(Self::all(cx), id)
    }

    /// The remote a workspace points at, or `None` when it is a local one.
    pub fn remote_ref(cx: &gpui::App, id: WorkspaceId) -> Option<RemoteRef> {
        Self::all(cx).get(id).and_then(|w| w.host.clone())
    }

    /// The client-side entry for `host` — the existing one if this machine has
    /// seen that workspace before, a fresh one otherwise.
    ///
    /// The two ids are deliberately different things: the entry has its own
    /// [`WorkspaceId`] (this client's handle, what the window registry and the
    /// Window menu key on), and `host.workspace` is the id **on the remote**,
    /// which is what the `WorkspacePut` / `WorkspaceGet` calls carry. Reusing
    /// one id for both would collide the moment two machines minted the same
    /// uuid, and would quietly make a client id meaningful off this machine.
    ///
    /// The entry is matched on the whole [`RemoteRef`], so the same workspace id
    /// on two different machines is two entries, and reconnecting to one you
    /// have opened before reuses its window geometry rather than cascading a new
    /// window every time.
    pub fn claim_remote(cx: &mut gpui::App, host: RemoteRef) -> WorkspaceId {
        let Some(store) = Self::try_store(cx) else {
            return WorkspaceId::new();
        };
        let existing = store
            .workspaces
            .workspaces
            .iter()
            .find(|w| w.host.as_ref() == Some(&host))
            .map(|w| w.id);
        let id = match existing {
            Some(id) => id,
            None => {
                let workspace = Workspace::on_remote(host);
                let id = workspace.id;
                store.workspaces.workspaces.push(workspace);
                id
            }
        };
        store.workspaces.save();
        id
    }

    /// Merge an authoritative record pulled from the remote into the client's
    /// entry. Only the remote-owned fields move; `open`, `window` and `host`
    /// stay as this machine left them (see [`Workspace::apply_remote_json`]).
    ///
    /// A record that will not decode is dropped with a log line rather than
    /// failing the open: the layout is recoverable on the next push, an
    /// unopenable workspace is not.
    pub fn apply_remote(cx: &mut gpui::App, id: WorkspaceId, record: &serde_json::Value) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        let Some(workspace) = store.workspaces.get_mut(id) else {
            return;
        };
        if let Err(e) = workspace.apply_remote_json(record) {
            log::warn!("remote workspace {id} sent a record this build cannot read: {e}");
            return;
        }
        store.workspaces.save();
    }

    /// What to send the remote for `id`: its store key and the remote-owned half
    /// of the record. `None` for a local workspace — there is nobody to send to.
    pub fn remote_payload(
        cx: &gpui::App,
        id: WorkspaceId,
    ) -> Option<(RemoteRef, String, serde_json::Value)> {
        let workspace = Self::all(cx).get(id)?;
        let host = workspace.host.clone()?;
        let key = host.store_key();
        // The record travels under the *remote's* id, not the client entry's:
        // the remote store is keyed by its own ids, and a record whose `id`
        // disagreed with its key would be a workspace that renames itself on
        // every round trip.
        let mut record = workspace.to_remote_json();
        if let Some(obj) = record.as_object_mut() {
            obj.insert("id".to_string(), serde_json::json!(key));
        }
        Some((host, key, record))
    }
}

/// Write a window's layout onto its workspace entry, honouring design §10's
/// storage split.
///
/// **A remote workspace's entry never holds a layout on this client.** The
/// machine's own `workspaces.json` is the authority for it, and the client entry
/// is a pointer plus this machine's view state. That is not just tidiness: a
/// remote entry carrying local `SessionPane`s is exactly the shape "one window,
/// two hosts" would take on disk, and clearing it here is what makes the
/// invariant survive a restart rather than only holding while the app runs.
/// The machine a window showing `id` is bound to.
///
/// The whole of "one window, one machine" reduces to this being a *function*: a
/// window shows one workspace, a workspace names one host, so a window has one
/// host and there is no arrangement of the data in which it has two. Split out
/// from [`WorkspaceStore::host_of`] so it can be tested against a workspace set
/// built by hand, with no globals and nothing written to disk.
///
/// An id that is not on file answers `LOCAL`: a window whose workspace was
/// deleted out from under it is showing nothing, and "nothing" is here — the
/// safe answer, because it is the one that refuses no local action.
pub(crate) fn host_for(workspaces: &Workspaces, id: WorkspaceId) -> HostId {
    workspaces
        .get(id)
        .map(|w| w.host_id())
        .unwrap_or(HostId::LOCAL)
}

/// Whether rebinding a window from `previous` to `current` moved it to another
/// machine — the moment every piece of per-*window* state that outlived the
/// swap has to be reconsidered.
pub(crate) fn crosses_machines(previous: HostId, current: HostId) -> bool {
    previous != current
}

/// The layout a window opening on `workspace` may rebuild — the read-side twin
/// of [`record_session`].
///
/// Both halves are needed, and it took a real launch to notice: the write guard
/// stops this client *creating* a remote entry with a local layout, but it says
/// nothing about one that arrived some other way — a hand-edited `session.json`,
/// a file written before the split existed, a sync tool. Restoring such an entry
/// would rebuild local shells inside a window bound to another machine, which is
/// design §3's "never do this" arriving through the back door.
///
/// So a remote workspace always opens empty here, and the entry is scrubbed on
/// the way past so the bad layout does not survive to be tried again. The real
/// layout is the remote's `workspaces.json`, pulled on connect.
fn claimable_session(workspace: &mut Workspace) -> Session {
    if workspace.is_remote() {
        workspace.session = Session::default();
        return Session::default();
    }
    workspace.session.clone()
}

fn record_session(workspace: &mut Workspace, session: Session) {
    workspace.session = if workspace.is_remote() {
        Session::default()
    } else {
        session
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(cwd: &str) -> SessionPane {
        SessionPane::Leaf {
            cwd: Some(std::path::PathBuf::from(cwd)),
            pane_id: Some(7),
            ssh_spec: None,
            agent: None,
            agent_session_id: None,
            agent_launch_argv: None,
        }
    }

    fn local_layout() -> Session {
        Session {
            tabs: vec![SessionTab {
                name: None,
                sidebar_group: None,
                pane: leaf("/Users/me/work"),
            }],
            ..Session::default()
        }
    }

    fn remote_ref() -> RemoteRef {
        RemoteRef::new(
            RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            WorkspaceId::new(),
        )
    }

    /// A local workspace records its layout the way it always did.
    #[test]
    fn a_local_workspace_stores_its_own_layout() {
        let mut workspace = Workspace::default();
        record_session(&mut workspace, local_layout());
        assert_eq!(workspace.session.tabs.len(), 1);
        assert_eq!(workspace.pane_ids(), vec![7]);
    }

    /// The one that matters: a remote entry must never end up holding panes
    /// from this machine. This is the on-disk half of "a window is one machine"
    /// — if a local layout could be written onto a remote entry, the next launch
    /// would restore local shells into a window bound to a remote host, which is
    /// design §3's "never do this".
    #[test]
    fn a_remote_workspace_never_stores_a_local_layout() {
        let mut workspace = Workspace::on_remote(remote_ref());
        record_session(&mut workspace, local_layout());
        assert!(workspace.session.tabs.is_empty());
        assert!(workspace.pane_ids().is_empty());
    }

    /// A local workspace opens on the layout it saved.
    #[test]
    fn a_local_workspace_reopens_its_saved_layout() {
        let mut workspace = Workspace {
            session: local_layout(),
            ..Workspace::default()
        };
        let claimed = claimable_session(&mut workspace);
        assert_eq!(claimed.tabs.len(), 1);
        // And the entry is left alone.
        assert_eq!(workspace.session.tabs.len(), 1);
    }

    /// The regression a real launch caught: a remote entry that arrived holding
    /// a local layout — a hand-edited `session.json`, or a file written before
    /// the storage split — would otherwise rebuild local shells inside a window
    /// bound to another machine on the next start.
    ///
    /// It must open empty *and* be scrubbed, so a layout that got in somehow
    /// cannot sit there being retried on every launch.
    #[test]
    fn a_remote_workspace_never_reopens_a_local_layout() {
        let mut workspace = Workspace::on_remote(remote_ref());
        workspace.session = local_layout();

        let claimed = claimable_session(&mut workspace);
        assert!(claimed.tabs.is_empty(), "the window must open with no tabs");
        assert!(
            workspace.session.tabs.is_empty(),
            "and the bad layout must not survive to be tried again"
        );
    }

    /// And a remote entry that somehow *arrived* holding a layout (a
    /// hand-edited `session.json`, a record from a build that predates the
    /// split) is cleaned out the first time the window records itself, rather
    /// than being left to restore later.
    #[test]
    fn recording_clears_a_layout_a_remote_entry_should_never_have_had() {
        let mut workspace = Workspace::on_remote(remote_ref());
        workspace.session = local_layout();
        record_session(&mut workspace, Session::default());
        assert!(workspace.session.tabs.is_empty());
    }

    /// The remote-bound payload travels under the *remote's* id, so a record
    /// pushed and pulled back names the same workspace both times.
    #[test]
    fn the_remote_payload_is_keyed_by_the_remote_id_not_the_client_entry() {
        let host = remote_ref();
        let workspace = Workspace::on_remote(host.clone());
        let mut record = workspace.to_remote_json();
        record
            .as_object_mut()
            .unwrap()
            .insert("id".into(), serde_json::json!(host.store_key()));
        assert_eq!(host.store_key(), host.workspace.to_string());
        assert_ne!(host.store_key(), workspace.id.to_string());
        assert_eq!(record["id"], serde_json::json!(host.store_key()));
        // The client-owned half never crosses.
        for client_only in tty7_core::core::session::CLIENT_OWNED_FIELDS {
            assert!(
                record.get(*client_only).is_none(),
                "{client_only} must not be sent to the remote"
            );
        }
    }

    /// **The window/host invariant, as a test.**
    ///
    /// Design §2: a window is one machine. Design §3 puts the inverse under
    /// *never do this*, and the M5 data layer spends that guarantee — a
    /// workspace stores `host` once instead of per pane, and `sidebar_group`
    /// stays a bare `PathBuf` — so it has to be nailed down rather than
    /// believed.
    ///
    /// What is actually being asserted: for any workspace set containing local
    /// and remote entries on several machines, the host a window binds to is a
    /// *function* of the workspace it shows. Every id answers exactly one
    /// machine, and no id answers two.
    #[test]
    fn a_window_binds_to_exactly_one_machine() {
        let build = RemoteTarget::Alias {
            alias: "build-box".into(),
        };
        let gpu = RemoteTarget::direct("me", "gpu.lab", 2222);

        let local = Workspace::default();
        let build_a = Workspace::on_remote(RemoteRef::new(build.clone(), WorkspaceId::new()));
        let build_b = Workspace::on_remote(RemoteRef::new(build, WorkspaceId::new()));
        let gpu_a = Workspace::on_remote(RemoteRef::new(gpu, WorkspaceId::new()));
        let (local_id, build_a_id, build_b_id, gpu_id) =
            (local.id, build_a.id, build_b.id, gpu_a.id);

        let workspaces = Workspaces {
            workspaces: vec![local, build_a, build_b, gpu_a],
            ..Workspaces::default()
        };

        // Three machines are represented, and they stay apart.
        let l = host_for(&workspaces, local_id);
        let b1 = host_for(&workspaces, build_a_id);
        let b2 = host_for(&workspaces, build_b_id);
        let g = host_for(&workspaces, gpu_id);
        assert_eq!(l, HostId::LOCAL);
        assert_eq!(b1, b2, "two workspaces on one box share its connection");
        assert_ne!(b1, g);
        assert_ne!(b1, l);
        assert_ne!(g, l);

        // The answer is stable: asking twice cannot give a window a second host.
        assert_eq!(host_for(&workspaces, build_a_id), b1);

        // And a window whose workspace was deleted underneath it falls back to
        // local rather than to some other machine's id.
        assert_eq!(host_for(&workspaces, WorkspaceId::new()), HostId::LOCAL);

        // Only a host change is a machine change — the trigger for dropping the
        // per-window state (the closed-tab stack) that could otherwise carry a
        // tab across.
        assert!(!crosses_machines(b1, b2));
        assert!(crosses_machines(l, b1));
        assert!(crosses_machines(b1, g));
    }

    /// Two workspaces on one machine answer one `HostId`; a workspace on another
    /// machine answers a different one. That equality is what every "is this the
    /// same machine?" check in the window layer is built on.
    #[test]
    fn host_ids_group_by_machine_not_by_workspace() {
        let build = RemoteTarget::Alias {
            alias: "build-box".into(),
        };
        let other = RemoteTarget::Alias {
            alias: "other-box".into(),
        };
        let a = Workspace::on_remote(RemoteRef::new(build.clone(), WorkspaceId::new()));
        let b = Workspace::on_remote(RemoteRef::new(build, WorkspaceId::new()));
        let c = Workspace::on_remote(RemoteRef::new(other, WorkspaceId::new()));
        let local = Workspace::default();

        assert_eq!(a.host_id(), b.host_id());
        assert_ne!(a.host_id(), c.host_id());
        assert_eq!(local.host_id(), HostId::LOCAL);
        assert_ne!(a.host_id(), HostId::LOCAL);
    }
}
