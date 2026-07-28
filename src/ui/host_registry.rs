//! [`HostRegistry`] — the process's map from [`HostId`] to the live
//! [`SharedHost`] it names.
//!
//! A workspace holds its own `Arc<dyn Host>`, so most code never needs this.
//! The registry exists for the places that *cannot* hold an `Arc`: the git
//! status cache's path tables, pane records, anything persisted or keyed. Those
//! store a [`HostId`] — small, `Copy`, `Hash` — and come here when they need the
//! machine behind it.
//!
//! One entry per machine, not per workspace. Two workspaces on the same remote
//! box share a connection, share an id and share the host object; that is the
//! same granularity the SSH connection itself is pooled at, and it is why the
//! git cache can serve both from one probe.
//!
//! # Lifetime
//!
//! [`HostId::LOCAL`] is always present — the registry constructs itself with it,
//! so there is no initialization order to get wrong and no window in which a
//! local lookup can fail. Remote hosts are registered when their workspace
//! connects and dropped when the last workspace using them closes; a lookup for
//! an id that has gone away returns `None`, which call sites read as "that
//! machine is no longer around" and use to drop their cached rows.

use std::collections::HashMap;

use gpui::{App, Global};
use tty7_core::host::local::LocalHost;

pub use tty7_core::host::{Host, HostId, SharedHost};

/// Every host this process currently knows, by id.
pub struct HostRegistry {
    hosts: HashMap<HostId, SharedHost>,
}

impl Global for HostRegistry {}

impl Default for HostRegistry {
    fn default() -> Self {
        // The local host is not registered by anyone — it is a precondition. A
        // registry that could be missing it would force every `local()` call
        // site to handle an impossible `None`.
        let mut hosts = HashMap::new();
        hosts.insert(HostId::LOCAL, LocalHost::shared());
        HostRegistry { hosts }
    }
}

impl HostRegistry {
    /// The host `id` names, or `None` when it is no longer registered.
    ///
    /// `None` is not an error: a remote workspace that closed takes its host
    /// with it, and a cache entry still holding that id is simply stale.
    pub fn get(cx: &mut App, id: HostId) -> Option<SharedHost> {
        cx.default_global::<HostRegistry>().hosts.get(&id).cloned()
    }

    /// [`get`](Self::get) for the read-only half of the frame — the render and
    /// menu paths, which hold a `&App` and cannot create the global.
    ///
    /// A registry that does not exist yet answers for [`HostId::LOCAL`] anyway:
    /// [`LocalHost::shared`] is a process singleton, so the host handed back is
    /// the same object `default()` would have put in the map, gitignore cache
    /// and all. Every other id answers `None` until something registers it,
    /// which is the truth — nothing has connected to that machine.
    pub fn lookup(cx: &App, id: HostId) -> Option<SharedHost> {
        match cx.try_global::<HostRegistry>() {
            Some(reg) => reg.hosts.get(&id).cloned(),
            None => id.is_local().then(LocalHost::shared),
        }
    }

    /// This machine. Always present, so this cannot fail.
    pub fn local(cx: &mut App) -> SharedHost {
        HostRegistry::get(cx, HostId::LOCAL).expect("the local host is always registered")
    }

    /// Register `host` under its own id, replacing any previous host with that
    /// id (a reconnect mints a new host object for the same machine).
    ///
    /// Returns the host that was there before, so a caller can tell a reconnect
    /// from a first connection.
    pub fn insert(cx: &mut App, host: SharedHost) -> Option<SharedHost> {
        let id = host.id();
        cx.default_global::<HostRegistry>().hosts.insert(id, host)
    }

    /// Forget `id`. Refuses to forget the local host — nothing in the process
    /// can function without it, and a stray `remove` would turn every later
    /// local lookup into a panic far from the cause.
    pub fn remove(cx: &mut App, id: HostId) -> Option<SharedHost> {
        if id.is_local() {
            return None;
        }
        cx.default_global::<HostRegistry>().hosts.remove(&id)
    }

    /// Every registered id. Diagnostics and teardown; not a hot path.
    pub fn ids(cx: &mut App) -> Vec<HostId> {
        let mut ids: Vec<HostId> = cx
            .default_global::<HostRegistry>()
            .hosts
            .keys()
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    /// How many hosts are registered, local included.
    pub fn len(cx: &mut App) -> usize {
        cx.default_global::<HostRegistry>().hosts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is usable the instant it is touched: no init call, no
    /// ordering constraint, and the local host already in it.
    #[test]
    fn a_default_registry_already_holds_the_local_host() {
        let reg = HostRegistry::default();
        let local = reg.hosts.get(&HostId::LOCAL).expect("local is present");
        assert!(local.id().is_local());
        assert_eq!(reg.hosts.len(), 1);
    }

    /// Registering, replacing and removing a remote host — and the guarantee
    /// that `remove` cannot take the local one out from under the process.
    #[test]
    fn remote_hosts_come_and_go_but_local_stays() {
        let mut reg = HostRegistry::default();
        // A stand-in for a remote host: `LocalHost` under a remote id is enough
        // to exercise the map, and does not need a server to talk to.
        let id = HostId::from_connection_key("ssh-direct:me@box:22");
        reg.hosts.insert(id, LocalHost::new());
        assert_eq!(reg.hosts.len(), 2);

        assert!(reg.hosts.remove(&id).is_some());
        assert!(reg.hosts.contains_key(&HostId::LOCAL));
        assert!(reg.hosts.get(&id).is_none());
    }
}
