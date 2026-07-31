use anyhow::{Result, bail};
use serde_json::{Value, json};
use tty7_core::core::machine::{Axis, Machine, PaneSeed, Workspace};
use tty7_core::core::session::WorkspaceId;
use tty7_core::daemon::control::{
    CONTROL_VERSION, ControlEvent, ControlRequest, ReplyOk,
};
use tty7_core::daemon::protocol::PROTOCOL_VERSION;

use crate::address::{self, Address, Context, WorkspaceAddress};
use crate::backend::{Backend, RunSpec};
use crate::cli::{
    CaptureArgs, Cli, Command, MachineCmd, PaneCmd, RunArgs, SendArgs, ServerCmd, SplitArgs,
    TabCmd, WsCmd,
};
use crate::output;
use crate::resolve;

#[derive(Debug)]
pub struct Report {
    pub human: String,
    pub json: Value,
}

#[derive(Debug)]
pub enum Outcome {
    Report(Report),
    Exit(i32),
}

fn report(human: impl Into<String>, json: Value) -> Result<Outcome> {
    Ok(Outcome::Report(Report {
        human: human.into(),
        json,
    }))
}

pub fn execute(cli: Cli, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let json_mode = cli.json;
    match cli.command {
        None => launch_gui(cli.path),
        Some(Command::Ls) | Some(Command::Ws(WsCmd::Ls)) => ws_ls(backend),
        Some(Command::Ws(WsCmd::Tree { ws })) => ws_tree(ws.as_deref(), ctx, backend),
        Some(Command::Ws(WsCmd::New { name })) => ws_new(name, backend),
        Some(Command::Ws(WsCmd::Rename { ws, name })) => ws_rename(&ws, name, backend),
        Some(Command::Ws(WsCmd::Stop { .. })) => bail!(
            "`tty7 ws stop` is not implemented yet — the control dialect has no \
             workspace-stop request; it arrives with the multi-subscriber slice"
        ),
        Some(Command::Ws(WsCmd::Rm { ws })) => ws_rm(&ws, backend),
        Some(Command::Ws(WsCmd::Attach { ws })) => {
            ws_attach(address::parse_workspace(&ws), backend)
        }
        Some(Command::Ws(WsCmd::Detach { ws })) => ws_detach(&ws, backend),
        Some(Command::New { path }) => new_workspace(path, backend),
        Some(Command::Attach { target }) => attach(&target, backend),
        Some(Command::Run(args)) => run(args, ctx, backend),
        Some(Command::Split(args)) | Some(Command::Pane(PaneCmd::Split(args))) => {
            pane_split(args, ctx, backend)
        }
        Some(Command::Send(args)) => send(args, ctx, backend),
        Some(Command::Capture(args)) => capture(args, ctx, backend),
        Some(Command::Procs { target }) => procs(target.as_deref(), ctx, backend),
        Some(Command::Tab(TabCmd::Ls { ws })) => tab_ls(ws.as_deref(), ctx, backend),
        Some(Command::Tab(TabCmd::New { ws, cwd })) => tab_new(ws.as_deref(), cwd, ctx, backend),
        Some(Command::Tab(TabCmd::Close { tab })) => tab_close(&tab, backend),
        Some(Command::Tab(TabCmd::Rename { tab, name })) => tab_rename(&tab, name, backend),
        Some(Command::Tab(TabCmd::Move { tab, index })) => tab_move(&tab, index, backend),
        Some(Command::Pane(PaneCmd::Ls { ws })) => pane_ls(ws.as_deref(), backend),
        Some(Command::Pane(PaneCmd::Close { target })) => pane_close(target.as_deref(), ctx, backend),
        Some(Command::Events) => events(json_mode, backend),
        Some(Command::Agents) => agents(backend),
        Some(Command::Status) | Some(Command::Server(ServerCmd::Status)) => status(backend),
        Some(Command::Machine(MachineCmd::Ls)) => machine_ls(backend),
        Some(Command::Machine(MachineCmd::Connect { .. }))
        | Some(Command::Machine(MachineCmd::Disconnect { .. })) => bail!(
            "managing machine links from the CLI is not implemented yet — \
             use the GUI's connection manager for now"
        ),
        Some(Command::Server(ServerCmd::Start)) => crate::server::start(),
        Some(Command::Server(ServerCmd::Stop)) => crate::server::stop(),
        Some(Command::Server(ServerCmd::Restart)) => crate::server::restart(),
        Some(Command::Server(ServerCmd::Logs)) => crate::server::logs(),
        Some(Command::Doctor) => doctor(ctx, backend),
    }
}

fn launch_gui(path: Option<String>) -> Result<Outcome> {
    match path {
        Some(p) => bail!("launching the GUI is not wired up yet (would open {p})"),
        None => bail!("launching the GUI is not wired up yet"),
    }
}

fn fetch_machine(backend: &mut dyn Backend) -> Result<Machine> {
    match backend.control(ControlRequest::MachineGet)? {
        ReplyOk::MachineTree(m) => Ok(*m),
        other => bail!("the server answered MachineGet with {other:?}"),
    }
}

fn workspace_summary(ws: &Workspace) -> Value {
    json!({
        "id": ws.id.to_string(),
        "name": ws.name,
        "tabs": ws.tabs.len(),
        "panes": ws.tabs.iter().map(|t| t.root.pane_ids().len()).sum::<usize>(),
        "attached": ws.attachment.as_ref().map(|a| a.hostname.clone()),
    })
}

fn ws_ls(backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let summaries: Vec<Value> = machine.workspaces.iter().map(workspace_summary).collect();
    report(
        output::workspace_table(&machine),
        json!({ "workspaces": summaries }),
    )
}

fn resolve_ws(
    explicit: Option<&str>,
    ctx: &Context,
    machine: &Machine,
) -> Result<WorkspaceId> {
    let addr = address::workspace_or_context(explicit, ctx)?;
    Ok(resolve::workspace(machine, &addr)?.id)
}

fn ws_tree(explicit: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve_ws(explicit, ctx, &machine)?;
    match backend.control(ControlRequest::WorkspaceTree { workspace: id })? {
        ReplyOk::WorkspaceTree(ws) => report(
            output::workspace_tree(&ws, &machine),
            serde_json::to_value(&*ws)?,
        ),
        other => bail!("the server answered WorkspaceTree with {other:?}"),
    }
}

fn ws_new(name: Option<String>, backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::WorkspaceCreate {
        name,
        workspace: None,
    })? {
        ReplyOk::WorkspaceTree(ws) => report(
            ws.id.to_string(),
            json!({ "id": ws.id.to_string(), "name": ws.name }),
        ),
        other => bail!("the server answered WorkspaceCreate with {other:?}"),
    }
}

fn ws_rename(ws: &str, name: String, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &address::parse_workspace(ws))?.id;
    backend.control(ControlRequest::WorkspaceRename {
        workspace: id,
        name: Some(name.clone()),
    })?;
    report("", json!({ "id": id.to_string(), "name": name }))
}

fn ws_rm(ws: &str, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &address::parse_workspace(ws))?.id;
    backend.control(ControlRequest::WorkspaceRemove { workspace: id })?;
    report("", json!({ "removed": id.to_string() }))
}

fn ws_attach(addr: WorkspaceAddress, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &addr)?.id;
    match backend.control(ControlRequest::WorkspaceAttach { id: id.to_string() })? {
        ReplyOk::Attached { took_over_from } => {
            let human = took_over_from
                .as_ref()
                .map(|host| format!("took over from {host}"))
                .unwrap_or_default();
            report(
                human,
                json!({ "attached": id.to_string(), "took_over_from": took_over_from }),
            )
        }
        other => bail!("the server answered WorkspaceAttach with {other:?}"),
    }
}

fn ws_detach(ws: &str, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve::workspace(&machine, &address::parse_workspace(ws))?.id;
    backend.control(ControlRequest::WorkspaceDetach { id: id.to_string() })?;
    report("", json!({ "detached": id.to_string() }))
}

fn new_workspace(path: Option<String>, backend: &mut dyn Backend) -> Result<Outcome> {
    let ws = match backend.control(ControlRequest::WorkspaceCreate {
        name: None,
        workspace: None,
    })? {
        ReplyOk::WorkspaceTree(ws) => *ws,
        other => bail!("the server answered WorkspaceCreate with {other:?}"),
    };
    let pane = backend.spawn_shell(ws.id, path.clone())?;
    backend.control(ControlRequest::TabCreate {
        workspace: ws.id,
        at: None,
        pane: PaneSeed {
            pane,
            cwd: path,
            ssh_spec: None,
            agent: None,
        },
        tab: None,
    })?;
    report(
        ws.id.to_string(),
        json!({ "id": ws.id.to_string(), "pane": pane }),
    )
}

fn attach(target: &str, backend: &mut dyn Backend) -> Result<Outcome> {
    match address::parse(target)? {
        Address::Pane(pane) => {
            backend.attach_pane(pane)?;
            report("", json!({ "detached_from": pane }))
        }
        Address::Workspace(addr) => ws_attach(addr, backend),
        Address::Tab(_) => bail!("attach takes a %pane or a workspace, not a tab"),
    }
}

fn run(args: RunArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let workspace = match args.ws.as_deref() {
        Some(explicit) => {
            let machine = fetch_machine(backend)?;
            Some(resolve::workspace(&machine, &address::parse_workspace(explicit))?.id)
        }
        None => ctx.ws.as_deref().and_then(|v| v.parse::<WorkspaceId>().ok()),
    };
    let code = backend.run(RunSpec {
        workspace,
        cwd: args.cwd,
        command: args.cmd,
        keep: args.keep,
    })?;
    Ok(Outcome::Exit(code))
}

fn pane_split(args: SplitArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    let machine = fetch_machine(backend)?;
    let workspace = resolve::workspace_of_pane(&machine, pane)?.id;
    let cwd = machine
        .panes
        .iter()
        .find(|p| p.id == pane)
        .and_then(|p| p.cwd.clone());
    let axis = if args.horizontal {
        Axis::Horizontal
    } else {
        Axis::Vertical
    };
    let new = backend.spawn_shell(workspace, cwd.clone())?;
    backend.control(ControlRequest::PaneSplit {
        workspace,
        pane,
        axis,
        ratio: args.ratio,
        new: PaneSeed {
            pane: new,
            cwd,
            ssh_spec: None,
            agent: None,
        },
        first: false,
    })?;
    report(format!("%{new}"), json!({ "pane": new }))
}

fn send(args: SendArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let (target, text) = match &args.second {
        Some(text) => (Some(args.first.as_str()), text.as_str()),
        None => {
            if args.first.starts_with('%') && address::parse_pane(&args.first).is_ok() {
                bail!("send needs TEXT after the pane address");
            }
            (None, args.first.as_str())
        }
    };
    let pane = address::pane_or_context(target, ctx)?;
    let mut bytes = text.as_bytes().to_vec();
    if args.enter {
        bytes.push(b'\r');
    }
    backend.send_input(pane, bytes)?;
    report("", json!({ "pane": pane, "sent": text, "enter": args.enter }))
}

fn capture(args: CaptureArgs, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(args.target.as_deref(), ctx)?;
    let text = backend.capture(pane, args.scrollback)?;
    report(text.clone(), json!({ "pane": pane, "text": text }))
}

fn procs(target: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(target, ctx)?;
    let procs = backend.procs(pane)?;
    report(output::procs_tables(&procs), serde_json::to_value(&procs)?)
}

fn tab_ls(explicit: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve_ws(explicit, ctx, &machine)?;
    let ws = machine
        .workspaces
        .iter()
        .find(|ws| ws.id == id)
        .expect("resolve_ws returned an id straight out of this machine");
    let rows: Vec<Vec<String>> = ws
        .tabs
        .iter()
        .map(|tab| {
            vec![
                format!("@{}", resolve::ordinal_of(&machine, tab.id).unwrap_or(0)),
                tab.name.clone().unwrap_or_else(|| "-".to_string()),
                tab.root.pane_ids().len().to_string(),
            ]
        })
        .collect();
    let tabs: Vec<Value> = ws
        .tabs
        .iter()
        .map(|tab| {
            json!({
                "ordinal": resolve::ordinal_of(&machine, tab.id),
                "id": tab.id.to_string(),
                "name": tab.name,
                "panes": tab.root.pane_ids(),
            })
        })
        .collect();
    report(
        output::table(&["TAB", "NAME", "PANES"], &rows),
        json!({ "workspace": id.to_string(), "tabs": tabs }),
    )
}

fn tab_new(
    explicit: Option<&str>,
    cwd: Option<String>,
    ctx: &Context,
    backend: &mut dyn Backend,
) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let id = resolve_ws(explicit, ctx, &machine)?;
    let pane = backend.spawn_shell(id, cwd.clone())?;
    let tab = match backend.control(ControlRequest::TabCreate {
        workspace: id,
        at: None,
        pane: PaneSeed {
            pane,
            cwd,
            ssh_spec: None,
            agent: None,
        },
        tab: None,
    })? {
        ReplyOk::TabTree(tab) => *tab,
        other => bail!("the server answered TabCreate with {other:?}"),
    };
    report(
        format!("%{pane}"),
        json!({ "tab": tab.id.to_string(), "pane": pane }),
    )
}

fn tab_close(tab: &str, backend: &mut dyn Backend) -> Result<Outcome> {
    let addr = address::parse_tab(tab)?;
    let machine = fetch_machine(backend)?;
    let (workspace, tab) = resolve::tab(&machine, &addr)?;
    backend.control(ControlRequest::TabClose { workspace, tab })?;
    report("", json!({ "closed": tab.to_string() }))
}

fn tab_rename(tab: &str, name: String, backend: &mut dyn Backend) -> Result<Outcome> {
    let addr = address::parse_tab(tab)?;
    let machine = fetch_machine(backend)?;
    let (workspace, tab) = resolve::tab(&machine, &addr)?;
    backend.control(ControlRequest::TabRename {
        workspace,
        tab,
        name: Some(name.clone()),
    })?;
    report("", json!({ "tab": tab.to_string(), "name": name }))
}

fn tab_move(tab: &str, index: u64, backend: &mut dyn Backend) -> Result<Outcome> {
    let addr = address::parse_tab(tab)?;
    let machine = fetch_machine(backend)?;
    let (workspace, tab) = resolve::tab(&machine, &addr)?;
    backend.control(ControlRequest::TabMove {
        workspace,
        tab,
        to: index,
    })?;
    report("", json!({ "tab": tab.to_string(), "to": index }))
}

fn pane_ls(explicit: Option<&str>, backend: &mut dyn Backend) -> Result<Outcome> {
    let machine = fetch_machine(backend)?;
    let only = match explicit {
        Some(s) => Some(resolve::workspace(&machine, &address::parse_workspace(s))?.id),
        None => None,
    };
    let mut panes = Vec::new();
    for ws in &machine.workspaces {
        if only.is_some_and(|id| id != ws.id) {
            continue;
        }
        for tab in &ws.tabs {
            for pane in tab.root.pane_ids() {
                let record = machine.panes.iter().find(|p| p.id == pane);
                panes.push(json!({
                    "pane": pane,
                    "workspace": ws.id.to_string(),
                    "tab": tab.id.to_string(),
                    "cwd": record.and_then(|r| r.cwd.clone()),
                    "live": record.map(|r| r.live),
                }));
            }
        }
    }
    report(output::pane_table(&machine, only), json!({ "panes": panes }))
}

fn pane_close(target: Option<&str>, ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let pane = address::pane_or_context(target, ctx)?;
    let machine = fetch_machine(backend)?;
    let workspace = resolve::workspace_of_pane(&machine, pane)?.id;
    backend.control(ControlRequest::PaneClose { workspace, pane })?;
    report("", json!({ "closed": pane }))
}

fn events(json_mode: bool, backend: &mut dyn Backend) -> Result<Outcome> {
    backend.events(&mut |event| {
        if json_mode {
            println!("{}", serde_json::to_string(&event)?);
        } else {
            println!("{}", event_line(&event));
        }
        Ok(())
    })?;
    report("", Value::Null)
}

fn event_line(event: &ControlEvent) -> String {
    match event {
        ControlEvent::PaneExited { pane_id, code } => match code {
            Some(code) => format!("pane %{pane_id} exited with code {code}"),
            None => format!("pane %{pane_id} exited"),
        },
        ControlEvent::AgentStatus { pane_id, json } => {
            format!("pane %{pane_id} agent status: {json}")
        }
        ControlEvent::Preempted { workspace, by } => {
            format!("workspace {workspace} taken over by {by}")
        }
        ControlEvent::Layout { workspace, delta } => {
            format!("workspace {workspace} layout: {delta:?}")
        }
        ControlEvent::LayoutResync => "layout resync".to_string(),
        other => format!("{other:?}"),
    }
}

fn agents(backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::AgentStates)? {
        ReplyOk::AgentStates(states) => report(
            output::agents_table(&states),
            json!({ "agents": serde_json::to_value(&states)? }),
        ),
        other => bail!("the server answered AgentStates with {other:?}"),
    }
}

fn status(backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::Status)? {
        ReplyOk::Status(status) => {
            report(output::status_lines(&status), serde_json::to_value(&status)?)
        }
        other => bail!("the server answered Status with {other:?}"),
    }
}

fn machine_ls(backend: &mut dyn Backend) -> Result<Outcome> {
    match backend.control(ControlRequest::Routes)? {
        ReplyOk::Routes(routes) => report(
            output::routes_table(&routes),
            json!({ "machines": serde_json::to_value(&routes)? }),
        ),
        other => bail!("the server answered Routes with {other:?}"),
    }
}

fn doctor(ctx: &Context, backend: &mut dyn Backend) -> Result<Outcome> {
    let mark = |v: &Option<String>| match v {
        Some(value) => format!("set ({value})"),
        None => "missing".to_string(),
    };
    let mut rows = vec![
        vec![address::ENV_SOCKET.to_string(), mark(&ctx.socket)],
        vec![address::ENV_WS.to_string(), mark(&ctx.ws)],
        vec![address::ENV_PANE.to_string(), mark(&ctx.pane)],
    ];
    let mut server = json!({ "reachable": false });
    match backend.hello() {
        Ok(hello) => {
            let dialect_ok = hello.control_version == CONTROL_VERSION
                && hello.protocol_version == PROTOCOL_VERSION;
            let dialect = if dialect_ok {
                format!("ok (control v{CONTROL_VERSION}, protocol v{PROTOCOL_VERSION})")
            } else {
                format!(
                    "MISMATCH (server speaks control v{} protocol v{}, this build \
                     v{CONTROL_VERSION}/v{PROTOCOL_VERSION})",
                    hello.control_version, hello.protocol_version
                )
            };
            rows.push(vec!["server".to_string(), format!("ok (build {})", hello.build)]);
            rows.push(vec!["dialect".to_string(), dialect]);
            let status = match backend.control(ControlRequest::Status)? {
                ReplyOk::Status(status) => status,
                other => bail!("the server answered Status with {other:?}"),
            };
            rows.push(vec![
                "status".to_string(),
                format!(
                    "pid {}, up {}s, {} panes",
                    status.pid, status.uptime_secs, status.panes
                ),
            ]);
            let routes = match backend.control(ControlRequest::Routes)? {
                ReplyOk::Routes(routes) => routes,
                other => bail!("the server answered Routes with {other:?}"),
            };
            let connected = routes.iter().filter(|r| r.connected).count();
            rows.push(vec![
                "machine links".to_string(),
                format!("{} known, {connected} connected", routes.len()),
            ]);
            server = json!({
                "reachable": true,
                "dialect_ok": dialect_ok,
                "build": hello.build,
                "status": serde_json::to_value(&status)?,
                "routes": serde_json::to_value(&routes)?,
            });
        }
        Err(e) => {
            rows.push(vec!["server".to_string(), format!("unreachable — {e:#}")]);
        }
    }
    let mut human = output::table(&["CHECK", "RESULT"], &rows);
    if ctx.socket.is_none() && ctx.pane.is_none() {
        human.push_str(
            "\nnot inside a tty7 shell — address commands need an explicit %pane/@tab/workspace\n",
        );
    }
    report(
        human,
        json!({
            "context": {
                "socket": ctx.socket.is_some(),
                "workspace": ctx.ws.is_some(),
                "pane": ctx.pane.is_some(),
            },
            "server": server,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::testbed::two_workspace_machine;
    use clap::Parser;
    use tty7_core::core::machine::Tab;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("test invocations use the documented grammar")
    }

    fn mock() -> MockBackend {
        MockBackend::with_machine(two_workspace_machine())
    }

    fn run_cli(args: &[&str], ctx: &Context, backend: &mut MockBackend) -> Outcome {
        execute(cli(args), ctx, backend).expect("this command should succeed against the mock")
    }

    fn human(outcome: Outcome) -> String {
        match outcome {
            Outcome::Report(r) => r.human,
            Outcome::Exit(code) => panic!("expected a report, got exit {code}"),
        }
    }

    #[test]
    fn ls_and_ws_ls_are_the_same_request() {
        let ctx = Context::default();
        let mut a = mock();
        run_cli(&["tty7", "ls"], &ctx, &mut a);
        let mut b = mock();
        run_cli(&["tty7", "ws", "ls"], &ctx, &mut b);
        assert_eq!(a.control_calls, vec![ControlRequest::MachineGet]);
        assert_eq!(a.control_calls, b.control_calls, "the alias must not drift");
    }

    #[test]
    fn ws_tree_asks_for_the_resolved_workspace() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].clone();
        backend
            .replies
            .push_back(ReplyOk::WorkspaceTree(Box::new(api.clone())));
        run_cli(&["tty7", "ws", "tree", "api"], &Context::default(), &mut backend);
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::WorkspaceTree { workspace: api.id },
            ]
        );
    }

    #[test]
    fn ws_new_carries_the_name() {
        let mut backend = mock();
        let created = Workspace::default();
        backend
            .replies
            .push_back(ReplyOk::WorkspaceTree(Box::new(created.clone())));
        let out = run_cli(&["tty7", "ws", "new", "dev"], &Context::default(), &mut backend);
        assert_eq!(
            backend.control_calls,
            vec![ControlRequest::WorkspaceCreate {
                name: Some("dev".into()),
                workspace: None,
            }]
        );
        assert_eq!(human(out), created.id.to_string(), "the id is the printed result");
    }

    #[test]
    fn new_spawns_first_and_seeds_the_tab_with_the_daemons_pane_id() {
        let mut backend = mock();
        let created = Workspace::default();
        backend
            .replies
            .push_back(ReplyOk::WorkspaceTree(Box::new(created.clone())));
        backend
            .replies
            .push_back(ReplyOk::TabTree(Box::new(Tab::leaf(6))));
        let out = run_cli(&["tty7", "new", "C:\\newproj"], &Context::default(), &mut backend);
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::WorkspaceCreate {
                    name: None,
                    workspace: None,
                },
                ControlRequest::TabCreate {
                    workspace: created.id,
                    at: None,
                    pane: PaneSeed {
                        pane: 6,
                        cwd: Some("C:\\newproj".into()),
                        ssh_spec: None,
                        agent: None,
                    },
                    tab: None,
                },
            ],
            "the daemon-assigned pane id (6) lands in the tree op, so the spawn came first"
        );
        assert_eq!(
            backend.spawned,
            vec![(created.id, Some("C:\\newproj".to_string()))],
            "the tree op alone leaves a dead pane — the shell must be spawned"
        );
        assert_eq!(human(out), created.id.to_string());
    }

    #[test]
    fn ws_rename_rm_attach_detach_build_their_requests() {
        let ctx = Context::default();
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let web = backend.machine.workspaces[1].id;

        run_cli(&["tty7", "ws", "rename", "api", "core"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceRename {
                workspace: api,
                name: Some("core".into()),
            }
        );

        backend.control_calls.clear();
        run_cli(&["tty7", "ws", "rm", "web"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceRemove { workspace: web }
        );

        backend.control_calls.clear();
        backend.replies.push_back(ReplyOk::Attached {
            took_over_from: Some("laptop".into()),
        });
        let out = run_cli(&["tty7", "ws", "attach", "api"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceAttach {
                id: api.to_string(),
            }
        );
        assert_eq!(human(out), "took over from laptop");

        backend.control_calls.clear();
        run_cli(&["tty7", "ws", "detach", "api"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceDetach {
                id: api.to_string(),
            }
        );
    }

    #[test]
    fn top_level_attach_routes_by_address_shape() {
        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Attached {
            took_over_from: None,
        });
        run_cli(&["tty7", "attach", "web"], &Context::default(), &mut backend);
        let id = backend.machine.workspaces[1].id;
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::WorkspaceAttach { id: id.to_string() }
        );

        let mut backend = mock();
        let err = execute(cli(&["tty7", "attach", "%42"]), &Context::default(), &mut backend)
            .expect_err("the mock cannot attach a pane");
        assert!(
            err.to_string().contains("attach"),
            "pane attach reached the backend seam: {err}"
        );
        assert!(backend.control_calls.is_empty(), "pane attach is not a control request");
    }

    #[test]
    fn tab_verbs_resolve_machine_wide_ordinals_to_real_ids() {
        let ctx = Context::default();
        let mut backend = mock();
        let web = backend.machine.workspaces[1].clone();

        run_cli(&["tty7", "tab", "close", "@3"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::TabClose {
                    workspace: web.id,
                    tab: web.tabs[0].id,
                },
            ]
        );

        backend.control_calls.clear();
        let api = backend.machine.workspaces[0].clone();
        run_cli(&["tty7", "tab", "rename", "@1", "build2"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabRename {
                workspace: api.id,
                tab: api.tabs[0].id,
                name: Some("build2".into()),
            }
        );

        backend.control_calls.clear();
        run_cli(&["tty7", "tab", "move", "@2", "0"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabMove {
                workspace: api.id,
                tab: api.tabs[1].id,
                to: 0,
            }
        );
    }

    #[test]
    fn tab_new_uses_the_workspace_from_the_environment() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].clone();
        backend
            .replies
            .push_back(ReplyOk::TabTree(Box::new(Tab::leaf(6))));
        let ctx = Context {
            ws: Some(api.id.to_string()),
            ..Context::default()
        };
        run_cli(&["tty7", "tab", "new", "--cwd", "C:\\elsewhere"], &ctx, &mut backend);
        assert_eq!(
            backend.control_calls[1],
            ControlRequest::TabCreate {
                workspace: api.id,
                at: None,
                pane: PaneSeed {
                    pane: 6,
                    cwd: Some("C:\\elsewhere".into()),
                    ssh_spec: None,
                    agent: None,
                },
                tab: None,
            }
        );
        assert_eq!(backend.spawned, vec![(api.id, Some("C:\\elsewhere".to_string()))]);
    }

    #[test]
    fn pane_split_builds_the_split_and_spawns_the_new_shell() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let out = run_cli(
            &["tty7", "pane", "split", "%2", "--v", "--ratio", "0.3"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::PaneSplit {
                    workspace: api,
                    pane: 2,
                    axis: Axis::Vertical,
                    ratio: 0.3,
                    new: PaneSeed {
                        pane: 6,
                        cwd: Some("C:\\proj".into()),
                        ssh_spec: None,
                        agent: None,
                    },
                    first: false,
                },
            ],
            "the new pane inherits the split pane's cwd"
        );
        assert_eq!(backend.spawned, vec![(api, Some("C:\\proj".to_string()))]);
        assert_eq!(human(out), "%6", "the new pane address is the printed result");
    }

    #[test]
    fn split_without_an_address_uses_the_pane_from_the_environment() {
        let mut backend = mock();
        let ctx = Context {
            pane: Some("5".into()),
            ..Context::default()
        };
        run_cli(&["tty7", "split", "--h"], &ctx, &mut backend);
        let web = backend.machine.workspaces[1].id;
        match &backend.control_calls[1] {
            ControlRequest::PaneSplit {
                workspace,
                pane,
                axis,
                ..
            } => {
                assert_eq!(*workspace, web);
                assert_eq!(*pane, 5);
                assert_eq!(*axis, Axis::Horizontal);
            }
            other => panic!("expected PaneSplit, got {other:?}"),
        }
    }

    #[test]
    fn pane_close_traces_the_pane_to_its_workspace() {
        let mut backend = mock();
        run_cli(&["tty7", "pane", "close", "%5"], &Context::default(), &mut backend);
        let web = backend.machine.workspaces[1].id;
        assert_eq!(
            backend.control_calls,
            vec![
                ControlRequest::MachineGet,
                ControlRequest::PaneClose {
                    workspace: web,
                    pane: 5,
                },
            ]
        );
    }

    #[test]
    fn send_reaches_the_pane_socket_seam_not_the_control_socket() {
        let mut backend = mock();
        run_cli(
            &["tty7", "send", "%1", "make -j8", "--enter"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(backend.sent, vec![(1, b"make -j8\r".to_vec())]);
        assert!(backend.control_calls.is_empty());

        let ctx = Context {
            pane: Some("%3".into()),
            ..Context::default()
        };
        backend.sent.clear();
        run_cli(&["tty7", "send", "echo hi"], &ctx, &mut backend);
        assert_eq!(backend.sent, vec![(3, b"echo hi".to_vec())]);
    }

    #[test]
    fn send_outside_a_shell_without_an_address_names_the_fix() {
        let mut backend = mock();
        let err = execute(
            cli(&["tty7", "send", "echo hi"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), address::OUTSIDE_SHELL);

        let err = execute(
            cli(&["tty7", "send", "%1"]),
            &Context::default(),
            &mut backend,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("TEXT"),
            "an address with nothing to send is a mistake, not empty input: {err}"
        );
    }

    #[test]
    fn capture_and_procs_are_wired_through_the_backend() {
        let mut backend = mock();
        backend.capture_text = "$ make\nok\n".into();
        let out = run_cli(
            &["tty7", "capture", "%2", "--scrollback"],
            &Context::default(),
            &mut backend,
        );
        assert_eq!(backend.captured, vec![(2, true)]);
        assert_eq!(human(out), "$ make\nok\n", "capture prints the pane verbatim");

        run_cli(&["tty7", "procs", "%1"], &Context::default(), &mut backend);
        assert_eq!(backend.procs_calls, vec![1]);
    }

    #[test]
    fn run_passes_the_command_and_its_exit_code_through() {
        let mut backend = mock();
        let api = backend.machine.workspaces[0].id;
        let ctx = Context {
            ws: Some(api.to_string()),
            ..Context::default()
        };
        let out = execute(
            cli(&["tty7", "run", "--keep", "--", "cargo", "test"]),
            &ctx,
            &mut backend,
        )
        .unwrap();
        assert_eq!(
            backend.runs,
            vec![RunSpec {
                workspace: Some(api),
                cwd: None,
                command: vec!["cargo".into(), "test".into()],
                keep: true,
            }]
        );
        assert!(matches!(out, Outcome::Exit(0)), "run's outcome is the child's exit code");
    }

    #[test]
    fn the_still_missing_verbs_say_so_without_touching_the_wire() {
        for (args, needle) in [
            (vec!["tty7", "ws", "stop", "api"], "not implemented"),
            (vec!["tty7", "machine", "connect", "devbox"], "not implemented"),
            (vec!["tty7", "machine", "disconnect", "devbox"], "not implemented"),
        ] {
            let mut backend = mock();
            let err = execute(cli(&args), &Context::default(), &mut backend)
                .expect_err("stubbed verbs must fail loudly, not pretend");
            assert!(
                err.to_string().contains(needle),
                "{args:?} should mention '{needle}': {err}"
            );
            assert!(
                backend.control_calls.is_empty(),
                "{args:?} must not invent protocol traffic"
            );
        }
    }

    #[test]
    fn agents_status_and_machine_ls_are_single_aggregate_requests() {
        use tty7_core::daemon::control::{RouteInfo, ServerStatus};

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::AgentStates(Vec::new()));
        let out = run_cli(&["tty7", "agents"], &Context::default(), &mut backend);
        assert_eq!(backend.control_calls, vec![ControlRequest::AgentStates]);
        assert!(human(out).contains("no agents"), "an empty panel says so");

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Status(ServerStatus {
            pid: 4242,
            uptime_secs: 61,
            panes: 3,
            control_version: CONTROL_VERSION,
            protocol_version: PROTOCOL_VERSION,
            build: "26.7.5".into(),
            socket: "127.0.0.1:5555".into(),
        }));
        let out = run_cli(&["tty7", "status"], &Context::default(), &mut backend);
        assert_eq!(backend.control_calls, vec![ControlRequest::Status]);
        let rendered = human(out);
        assert!(rendered.contains("4242"), "{rendered}");
        assert!(rendered.contains("61s"), "{rendered}");

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Routes(vec![RouteInfo {
            key: "me@build-box:22".into(),
            kind: "ssh".into(),
            connected: true,
        }]));
        let out = run_cli(&["tty7", "machine", "ls"], &Context::default(), &mut backend);
        assert_eq!(backend.control_calls, vec![ControlRequest::Routes]);
        let rendered = human(out);
        assert!(rendered.contains("local"), "machine 0 is always listed: {rendered}");
        assert!(rendered.contains("me@build-box:22"), "{rendered}");
    }

    fn doctor_backend() -> MockBackend {
        use tty7_core::daemon::control::ServerStatus;

        let mut backend = mock();
        backend.replies.push_back(ReplyOk::Status(ServerStatus {
            pid: 4242,
            uptime_secs: 61,
            panes: 3,
            control_version: CONTROL_VERSION,
            protocol_version: PROTOCOL_VERSION,
            build: "26.7.5".into(),
            socket: "127.0.0.1:5555".into(),
        }));
        backend.replies.push_back(ReplyOk::Routes(Vec::new()));
        backend
    }

    #[test]
    fn doctor_reports_the_injected_context_and_the_server_half() {
        let out = human(run_cli(&["tty7", "doctor"], &Context::default(), &mut doctor_backend()));
        assert!(out.contains("TTY7_SOCKET"), "{out}");
        assert!(out.contains("missing"), "{out}");
        assert!(out.contains("dialect"), "{out}");
        assert!(out.contains(&format!("control v{CONTROL_VERSION}")), "{out}");
        assert!(out.contains("pid 4242"), "{out}");
        assert!(out.contains("0 known"), "{out}");

        let ctx = Context {
            pane: Some("7".into()),
            ws: None,
            socket: Some("sock".into()),
        };
        let out = human(run_cli(&["tty7", "doctor"], &ctx, &mut doctor_backend()));
        assert!(out.contains("set (sock)"), "{out}");
    }
}
