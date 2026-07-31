use anyhow::Result;
use tty7_core::core::session::WorkspaceId;
use tty7_core::daemon::control::{ControlEvent, ControlHelloOk, ControlRequest, ReplyOk};
use tty7_core::daemon::protocol::PaneProcs;

mod real;

pub use real::RealBackend;

#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    pub workspace: Option<WorkspaceId>,
    pub cwd: Option<String>,
    pub command: Vec<String>,
    pub keep: bool,
}

pub trait Backend {
    fn control(&mut self, req: ControlRequest) -> Result<ReplyOk>;

    fn hello(&mut self) -> Result<ControlHelloOk>;

    fn spawn_shell(&mut self, workspace: WorkspaceId, cwd: Option<String>) -> Result<u64>;

    fn send_input(&mut self, pane: u64, bytes: Vec<u8>) -> Result<()>;

    fn capture(&mut self, pane: u64, scrollback: bool) -> Result<String>;

    fn procs(&mut self, pane: u64) -> Result<PaneProcs>;

    fn attach_pane(&mut self, pane: u64) -> Result<()>;

    fn run_spawn(&mut self, spec: RunSpec) -> Result<u64>;

    fn run_wait(&mut self) -> Result<Option<i32>>;

    fn events(&mut self, on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()>;
}

#[cfg(test)]
pub mod mock {
    use std::collections::VecDeque;

    use anyhow::{Result, anyhow};
    use tty7_core::core::machine::Machine;
    use tty7_core::core::session::WorkspaceId;
    use tty7_core::daemon::control::{
        CONTROL_VERSION, ControlEvent, ControlHelloOk, ControlRequest, ReplyOk, feature,
    };
    use tty7_core::daemon::protocol::{PROTOCOL_VERSION, PaneProcs};

    use super::{Backend, RunSpec};

    pub struct MockBackend {
        pub machine: Machine,
        pub replies: VecDeque<ReplyOk>,
        pub control_calls: Vec<ControlRequest>,
        pub spawned: Vec<(WorkspaceId, Option<String>)>,
        pub next_spawn_id: u64,
        pub sent: Vec<(u64, Vec<u8>)>,
        pub captured: Vec<(u64, bool)>,
        pub capture_text: String,
        pub procs_calls: Vec<u64>,
        pub procs_reply: PaneProcs,
        pub runs: Vec<RunSpec>,
        pub run_exit: Option<i32>,
        pub events: Vec<ControlEvent>,
    }

    impl Default for MockBackend {
        fn default() -> MockBackend {
            MockBackend {
                machine: Machine::default(),
                replies: VecDeque::new(),
                control_calls: Vec::new(),
                spawned: Vec::new(),
                next_spawn_id: 6,
                sent: Vec::new(),
                captured: Vec::new(),
                capture_text: String::new(),
                procs_calls: Vec::new(),
                procs_reply: PaneProcs::default(),
                runs: Vec::new(),
                run_exit: Some(0),
                events: Vec::new(),
            }
        }
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

        fn hello(&mut self) -> Result<ControlHelloOk> {
            Ok(ControlHelloOk {
                control_version: CONTROL_VERSION,
                protocol_version: PROTOCOL_VERSION,
                build: "mock".into(),
                separator: '\\',
                home: "C:\\Users\\mock".into(),
                features: vec![feature::CONTROL.into(), feature::MACHINE_TREE.into()],
                instance: "mock-instance".into(),
            })
        }

        fn spawn_shell(&mut self, workspace: WorkspaceId, cwd: Option<String>) -> Result<u64> {
            self.spawned.push((workspace, cwd));
            let id = self.next_spawn_id;
            self.next_spawn_id += 1;
            Ok(id)
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

        fn run_spawn(&mut self, spec: RunSpec) -> Result<u64> {
            self.runs.push(spec);
            let id = self.next_spawn_id;
            self.next_spawn_id += 1;
            Ok(id)
        }

        fn run_wait(&mut self) -> Result<Option<i32>> {
            Ok(self.run_exit)
        }

        fn events(&mut self, on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()> {
            for event in self.events.drain(..) {
                on_event(event)?;
            }
            Ok(())
        }
    }
}
