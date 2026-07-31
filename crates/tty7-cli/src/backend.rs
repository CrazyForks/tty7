use anyhow::{Result, bail};
use tty7_core::core::session::WorkspaceId;
use tty7_core::daemon::control::{ControlEvent, ControlRequest, ReplyOk};
use tty7_core::daemon::protocol::PaneProcs;

#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    pub workspace: Option<WorkspaceId>,
    pub cwd: Option<String>,
    pub command: Vec<String>,
    pub keep: bool,
}

pub trait Backend {
    fn control(&mut self, req: ControlRequest) -> Result<ReplyOk>;

    fn spawn_shell(&mut self, pane: u64, workspace: WorkspaceId, cwd: Option<String>)
    -> Result<()>;

    fn send_input(&mut self, pane: u64, bytes: Vec<u8>) -> Result<()>;

    fn capture(&mut self, pane: u64, scrollback: bool) -> Result<String>;

    fn procs(&mut self, pane: u64) -> Result<PaneProcs>;

    fn attach_pane(&mut self, pane: u64) -> Result<()>;

    fn run(&mut self, spec: RunSpec) -> Result<i32>;

    fn events(&mut self, on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()>;
}

pub struct StubBackend;

const NOT_WIRED: &str =
    "the tty7 CLI is not wired to a server yet — the transport client lands in the next slice";

impl Backend for StubBackend {
    fn control(&mut self, _req: ControlRequest) -> Result<ReplyOk> {
        bail!(NOT_WIRED)
    }

    fn spawn_shell(
        &mut self,
        _pane: u64,
        _workspace: WorkspaceId,
        _cwd: Option<String>,
    ) -> Result<()> {
        bail!(NOT_WIRED)
    }

    fn send_input(&mut self, _pane: u64, _bytes: Vec<u8>) -> Result<()> {
        bail!(NOT_WIRED)
    }

    fn capture(&mut self, _pane: u64, _scrollback: bool) -> Result<String> {
        bail!(NOT_WIRED)
    }

    fn procs(&mut self, _pane: u64) -> Result<PaneProcs> {
        bail!(NOT_WIRED)
    }

    fn attach_pane(&mut self, _pane: u64) -> Result<()> {
        bail!(NOT_WIRED)
    }

    fn run(&mut self, _spec: RunSpec) -> Result<i32> {
        bail!(NOT_WIRED)
    }

    fn events(&mut self, _on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()> {
        bail!(NOT_WIRED)
    }
}

#[cfg(test)]
pub mod mock {
    use std::collections::VecDeque;

    use anyhow::{Result, anyhow};
    use tty7_core::core::machine::Machine;
    use tty7_core::core::session::WorkspaceId;
    use tty7_core::daemon::control::{ControlEvent, ControlRequest, ReplyOk};
    use tty7_core::daemon::protocol::PaneProcs;

    use super::{Backend, RunSpec};

    #[derive(Default)]
    pub struct MockBackend {
        pub machine: Machine,
        pub replies: VecDeque<ReplyOk>,
        pub control_calls: Vec<ControlRequest>,
        pub spawned: Vec<(u64, WorkspaceId, Option<String>)>,
        pub sent: Vec<(u64, Vec<u8>)>,
        pub captured: Vec<(u64, bool)>,
        pub capture_text: String,
        pub procs_calls: Vec<u64>,
        pub procs_reply: PaneProcs,
        pub runs: Vec<RunSpec>,
    }

    impl MockBackend {
        pub fn with_machine(machine: Machine) -> MockBackend {
            MockBackend {
                machine,
                ..MockBackend::default()
            }
        }
    }

    impl Backend for MockBackend {
        fn control(&mut self, req: ControlRequest) -> Result<ReplyOk> {
            let is_machine_get = req == ControlRequest::MachineGet;
            self.control_calls.push(req);
            if is_machine_get {
                return Ok(ReplyOk::MachineTree(Box::new(self.machine.clone())));
            }
            Ok(self.replies.pop_front().unwrap_or(ReplyOk::Unit))
        }

        fn spawn_shell(
            &mut self,
            pane: u64,
            workspace: WorkspaceId,
            cwd: Option<String>,
        ) -> Result<()> {
            self.spawned.push((pane, workspace, cwd));
            Ok(())
        }

        fn send_input(&mut self, pane: u64, bytes: Vec<u8>) -> Result<()> {
            self.sent.push((pane, bytes));
            Ok(())
        }

        fn capture(&mut self, pane: u64, scrollback: bool) -> Result<String> {
            self.captured.push((pane, scrollback));
            Ok(self.capture_text.clone())
        }

        fn procs(&mut self, pane: u64) -> Result<PaneProcs> {
            self.procs_calls.push(pane);
            Ok(self.procs_reply.clone())
        }

        fn attach_pane(&mut self, _pane: u64) -> Result<()> {
            Err(anyhow!("the mock backend cannot attach"))
        }

        fn run(&mut self, spec: RunSpec) -> Result<i32> {
            self.runs.push(spec);
            Ok(0)
        }

        fn events(&mut self, _on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()> {
            Ok(())
        }
    }
}
