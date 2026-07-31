use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::json;
use tty7_core::client::{ControlClient, PaneClient};
use tty7_core::core::session::WorkspaceId;
use tty7_core::daemon::control::{
    ControlEvent, ControlHello, ControlHelloOk, ControlRequest, ReplyOk, RouteInfo,
};
use tty7_core::daemon::protocol::{DaemonMsg, PaneProcs, ShellSpec, WinSize};
use tty7_core::daemon::router::RouteTarget;

use super::{Backend, RunSpec};

const SESSION_SIZE: WinSize = WinSize {
    cols: 120,
    rows: 30,
    cell_w: 8,
    cell_h: 16,
};

const REPLAY_FIRST_WAIT: Duration = Duration::from_secs(10);
const REPLAY_SETTLE: Duration = Duration::from_millis(300);

const NOT_RUNNING: &str =
    "could not reach the tty7 server on this machine — `tty7 server start` brings one up";

pub struct RealBackend {
    machine: Option<String>,
    route: Option<RouteTarget>,
    control: Option<ControlClient>,
    panes: Option<PaneClient>,
}

impl RealBackend {
    pub fn new(machine: Option<String>) -> RealBackend {
        RealBackend {
            machine,
            route: None,
            control: None,
            panes: None,
        }
    }

    fn hello_msg() -> ControlHello {
        ControlHello::host_rpc(format!("tty7-cli-{}", std::process::id()), hostname())
    }

    fn route(&mut self) -> Result<Option<RouteTarget>> {
        let Some(name) = self.machine.clone() else {
            return Ok(None);
        };
        if let Some(target) = &self.route {
            return Ok(Some(target.clone()));
        }
        let local = ControlClient::connect(&Self::hello_msg()).context(NOT_RUNNING)?;
        let routes = match local
            .request(ControlRequest::Routes)
            .context("asking the local server for its machine links")?
        {
            ReplyOk::Routes(routes) => routes,
            other => bail!("the server answered Routes with {other:?}"),
        };
        local.close();
        let target = resolve_route(&name, &routes)?;
        self.route = Some(target.clone());
        Ok(Some(target))
    }

    fn control_client(&mut self) -> Result<&ControlClient> {
        if self.control.is_none() {
            let hello = Self::hello_msg();
            let client = match self.route()? {
                Some(target) => ControlClient::routed(target, &hello).with_context(|| {
                    format!(
                        "routing to machine '{}' through the local server",
                        self.machine.as_deref().unwrap_or_default()
                    )
                })?,
                None => ControlClient::connect(&hello).context(NOT_RUNNING)?,
            };
            self.control = Some(client);
        }
        Ok(self.control.as_ref().expect("just filled"))
    }

    fn pane_client(&mut self) -> Result<&PaneClient> {
        if self.panes.is_none() {
            let client = match self.route()? {
                Some(target) => PaneClient::routed(target),
                None => PaneClient::local(),
            };
            self.panes = Some(client);
        }
        Ok(self.panes.as_ref().expect("just filled"))
    }
}

impl Backend for RealBackend {
    fn control(&mut self, req: ControlRequest) -> Result<ReplyOk> {
        let reply = self.control_client()?.request(req)?;
        Ok(reply)
    }

    fn hello(&mut self) -> Result<ControlHelloOk> {
        Ok(self.control_client()?.hello().clone())
    }

    fn spawn_shell(&mut self, workspace: WorkspaceId, cwd: Option<String>) -> Result<u64> {
        let session = self
            .pane_client()?
            .spawn(
                cwd.map(PathBuf::from),
                SESSION_SIZE,
                None,
                Some("tty7-cli".into()),
                Some(workspace.to_string()),
            )
            .context("spawning a shell")?;
        let pane = session.pane_id();
        session.detach()?;
        Ok(pane)
    }

    fn send_input(&mut self, pane: u64, bytes: Vec<u8>) -> Result<()> {
        let mut session = self
            .pane_client()?
            .attach(pane, SESSION_SIZE)
            .with_context(|| format!("attaching to pane %{pane}"))?;
        session.input(&bytes)?;
        session.detach()?;
        Ok(())
    }

    fn capture(&mut self, pane: u64, scrollback: bool) -> Result<String> {
        let mut session = self
            .pane_client()?
            .observe(pane, SESSION_SIZE)
            .with_context(|| format!("observing pane %{pane}"))?;
        session.set_recv_timeout(Some(REPLAY_FIRST_WAIT))?;
        let mut snapshots: Vec<Vec<u8>> = Vec::new();
        loop {
            match session.recv() {
                Ok(DaemonMsg::Snapshot(bytes)) => {
                    snapshots.push(bytes);
                    session.set_recv_timeout(Some(REPLAY_SETTLE))?;
                }
                Ok(DaemonMsg::Output(_)) | Ok(DaemonMsg::Exited { .. }) => break,
                Ok(_) => session.set_recv_timeout(Some(REPLAY_SETTLE))?,
                Err(e) if timed_out(&e) => break,
                Err(e) => return Err(anyhow!(e).context("reading the pane replay")),
            }
        }
        let _ = session.detach();
        let bytes: Vec<u8> = if scrollback {
            snapshots.concat()
        } else {
            snapshots.pop().unwrap_or_default()
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn procs(&mut self, pane: u64) -> Result<PaneProcs> {
        let procs = self.pane_client()?.procs(pane)?;
        Ok(procs)
    }

    fn attach_pane(&mut self, _pane: u64) -> Result<()> {
        bail!("interactive `tty7 attach %pane` is not wired yet — it lands in the next slice")
    }

    fn run(&mut self, spec: RunSpec) -> Result<i32> {
        let (program, args) = spec
            .command
            .split_first()
            .ok_or_else(|| anyhow!("run needs a command after `--`"))?;
        let shell = ShellSpec {
            program: program.clone(),
            args: args.to_vec(),
            args_are_tty7_defaults: false,
        };
        let mut session = self
            .pane_client()?
            .spawn(
                spec.cwd.map(PathBuf::from),
                SESSION_SIZE,
                Some(shell),
                Some("tty7-cli".into()),
                spec.workspace.map(|ws| ws.to_string()),
            )
            .with_context(|| format!("spawning `{program}`"))?;
        let mut stdout = std::io::stdout().lock();
        let code = loop {
            match session.recv() {
                Ok(DaemonMsg::Output(bytes)) | Ok(DaemonMsg::Snapshot(bytes)) => {
                    stdout.write_all(&bytes)?;
                    stdout.flush()?;
                }
                Ok(DaemonMsg::Exited { code }) => break code.unwrap_or(1),
                Ok(_) => {}
                Err(e) => return Err(anyhow!(e).context("streaming the command's output")),
            }
        };
        if spec.keep {
            session.detach()?;
        } else {
            session.kill()?;
        }
        Ok(code)
    }

    fn events(&mut self, on_event: &mut dyn FnMut(ControlEvent) -> Result<()>) -> Result<()> {
        let client = self.control_client()?;
        for event in client.events() {
            on_event(event)?;
        }
        Ok(())
    }
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "tty7-cli".to_string())
}

fn resolve_route(name: &str, routes: &[RouteInfo]) -> Result<RouteTarget> {
    let matches: Vec<&RouteInfo> = routes.iter().filter(|r| route_matches(name, r)).collect();
    match matches.as_slice() {
        [one] => target_for(one),
        [] if routes.is_empty() => bail!(
            "the local server holds no machine links — connect one from the GUI first \
             (`tty7 machine ls` shows them)"
        ),
        [] => bail!(
            "no machine '{name}' — known machines: {}",
            keys(routes.iter())
        ),
        many => bail!(
            "'{name}' names {} machines — use the full key: {}",
            many.len(),
            keys(many.iter().copied())
        ),
    }
}

fn keys<'a>(routes: impl Iterator<Item = &'a RouteInfo>) -> String {
    routes
        .map(|r| r.key.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn route_matches(name: &str, route: &RouteInfo) -> bool {
    route.key == name || host_of(&route.key) == Some(name)
}

fn host_of(key: &str) -> Option<&str> {
    let first = key.split('|').next()?;
    let after_user = first.split('@').nth(1)?;
    after_user.split(':').next()
}

fn target_for(route: &RouteInfo) -> Result<RouteTarget> {
    if route.kind != "ssh" {
        bail!(
            "machine '{}' is a {} link — the CLI can only route over ssh links yet",
            route.key,
            route.kind
        );
    }
    if route.key.contains('|') {
        bail!(
            "machine '{}' is reached through a jump/proxy chain, which the CLI cannot \
             rebuild from the link key yet — use the GUI for this machine",
            route.key
        );
    }
    let (user, rest) = route
        .key
        .split_once('@')
        .ok_or_else(|| anyhow!("unrecognized machine key '{}'", route.key))?;
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("unrecognized machine key '{}'", route.key))?;
    let port: u16 = port
        .parse()
        .map_err(|_| anyhow!("unrecognized machine key '{}'", route.key))?;
    let spec = serde_json::from_value(json!({
        "user": user,
        "host": host,
        "port": port,
        "auth_mode": "auto",
    }))?;
    Ok(RouteTarget::Ssh(Box::new(spec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(key: &str, kind: &str, connected: bool) -> RouteInfo {
        RouteInfo {
            key: key.into(),
            kind: kind.into(),
            connected,
        }
    }

    #[test]
    fn a_machine_resolves_by_full_key_or_bare_host() {
        let routes = vec![
            route("me@build-box:22", "ssh", true),
            route("me@web-box:2222", "ssh", false),
        ];
        for name in ["me@build-box:22", "build-box"] {
            let RouteTarget::Ssh(spec) = resolve_route(name, &routes).unwrap() else {
                panic!("ssh routes resolve to ssh targets");
            };
            assert_eq!(spec.user, "me");
            assert_eq!(spec.host, "build-box");
            assert_eq!(spec.port, 22);
        }
        let RouteTarget::Ssh(spec) = resolve_route("web-box", &routes).unwrap() else {
            panic!("ssh routes resolve to ssh targets");
        };
        assert_eq!(spec.port, 2222);
    }

    #[test]
    fn unknown_and_ambiguous_names_list_the_candidates() {
        let routes = vec![
            route("a@shared:22", "ssh", true),
            route("b@shared:22", "ssh", true),
        ];
        let err = resolve_route("nowhere", &routes).unwrap_err().to_string();
        assert!(err.contains("a@shared:22"), "{err}");
        assert!(err.contains("b@shared:22"), "{err}");

        let err = resolve_route("shared", &routes).unwrap_err().to_string();
        assert!(err.contains("2 machines"), "{err}");

        let err = resolve_route("anything", &[]).unwrap_err().to_string();
        assert!(err.contains("no machine links"), "{err}");
    }

    #[test]
    fn chained_keys_are_refused_with_the_reason() {
        let routes = vec![route("me@inner:22|jump:me@bastion:22", "ssh", true)];
        let err = resolve_route("me@inner:22|jump:me@bastion:22", &routes)
            .unwrap_err()
            .to_string();
        assert!(err.contains("jump/proxy chain"), "{err}");
    }
}
