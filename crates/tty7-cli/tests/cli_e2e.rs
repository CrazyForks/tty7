use std::io::BufRead as _;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tty7_core::client::{ControlClient, PaneClient};
use tty7_core::daemon::control::ControlHello;
use tty7_core::daemon::protocol::PROTOCOL_VERSION;

const DAEMON_ENV: &str = "TTY7_CLI_E2E_DAEMON";
const READY_WITHIN: Duration = Duration::from_secs(30);
const SETTLE_WITHIN: Duration = Duration::from_secs(60);

fn main() {
    if std::env::var(DAEMON_ENV).as_deref() == Ok("1") {
        if let Err(e) = tty7_core::daemon::server::run_daemon() {
            eprintln!("e2e daemon exited with error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let tests: &[(&str, fn(&Daemon))] = &[
        ("ls_on_an_empty_server", ls_on_an_empty_server),
        ("new_builds_a_workspace_with_a_live_pane", new_builds_a_workspace_with_a_live_pane),
        ("run_streams_output_and_passes_the_exit_code", run_streams_output_and_passes_the_exit_code),
        ("send_then_capture_round_trip", send_then_capture_round_trip),
        ("status_reports_the_live_server", status_reports_the_live_server),
        ("events_stream_reports_a_workspace_creation", events_stream_reports_a_workspace_creation),
    ];

    let mut failed = 0;
    for (name, test) in tests {
        let daemon = Daemon::start();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&daemon)));
        drop(daemon);
        match outcome {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                failed += 1;
                println!("test {name} ... FAILED");
            }
        }
    }
    if failed > 0 {
        eprintln!("{failed} e2e test(s) failed");
        std::process::exit(1);
    }
}

struct Daemon {
    child: Child,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Daemon {
        let dir = tempfile::TempDir::new().expect("a temp dir for the isolated server");
        let own = std::env::current_exe().expect("own test binary path");
        let child = Command::new(own)
            .env(DAEMON_ENV, "1")
            .env("TTY7_CONFIG_DIR", dir.path())
            .env("TTY7_DATA_DIR", dir.path())
            .env("TTY7_CONTROL_SOCK", dir.path().join("control.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the in-test tty7 server");
        let daemon = Daemon { child, dir };
        daemon.await_ready();
        daemon
    }

    fn control_endpoint(&self) -> PathBuf {
        let file = if cfg!(windows) {
            "control.port"
        } else {
            "control.sock"
        };
        self.dir.path().join(file)
    }

    fn pane_endpoint(&self) -> PathBuf {
        let file = if cfg!(windows) {
            "daemon.port"
        } else {
            "daemon.sock"
        };
        self.dir.path().join(file)
    }

    fn await_ready(&self) {
        let hello = ControlHello::host_rpc("e2e-probe", "e2e-probe");
        let deadline = Instant::now() + READY_WITHIN;
        loop {
            let control_up = ControlClient::connect_at(&self.control_endpoint(), &hello).is_ok();
            let panes_up = PaneClient::at(self.pane_endpoint()).version().is_ok();
            if control_up && panes_up {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the isolated server did not open its endpoints within {READY_WITHIN:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn cli(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tty7"));
        cmd.args(args)
            .env("TTY7_CONFIG_DIR", self.dir.path())
            .env("TTY7_DATA_DIR", self.dir.path())
            .env("TTY7_CONTROL_SOCK", self.dir.path().join("control.sock"))
            .env_remove("TTY7_PANE")
            .env_remove("TTY7_WS")
            .env_remove("TTY7_SOCKET");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cli(args)
            .output()
            .unwrap_or_else(|e| panic!("could not run tty7 {args:?}: {e}"))
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "tty7 {args:?} failed ({}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn run_json(&self, args: &[&str]) -> serde_json::Value {
        let mut with_json: Vec<&str> = args.to_vec();
        with_json.push("--json");
        let out = self.run_ok(&with_json);
        serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("tty7 {args:?} --json printed no JSON ({e}): {out}"))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn workdir() -> String {
    std::env::temp_dir().display().to_string()
}

fn one_shot(command: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".into(), "/d".into(), "/c".into(), command.into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), command.into()]
    }
}

fn ls_on_an_empty_server(daemon: &Daemon) {
    let out = daemon.run_ok(&["ls"]);
    assert!(out.contains("no workspaces"), "{out}");
}

fn new_builds_a_workspace_with_a_live_pane(daemon: &Daemon) {
    let created = daemon.run_json(&["new", &workdir()]);
    let ws_id = created["id"].as_str().expect("new prints the workspace id");
    let pane = created["pane"].as_u64().expect("new prints the pane id");
    assert!(pane >= 1, "the daemon names panes from 1, got {pane}");

    let listed = daemon.run_json(&["ls"]);
    let workspaces = listed["workspaces"].as_array().expect("ls --json lists workspaces");
    assert_eq!(workspaces.len(), 1, "{listed}");
    assert_eq!(workspaces[0]["id"].as_str(), Some(ws_id), "{listed}");
    assert_eq!(workspaces[0]["panes"].as_u64(), Some(1), "{listed}");

    let panes = daemon.run_ok(&["pane", "ls"]);
    assert!(panes.contains(&format!("%{pane}")), "{panes}");
}

fn run_streams_output_and_passes_the_exit_code(daemon: &Daemon) {
    let echo = one_shot("echo tty7_e2e_run_marker");
    let mut args: Vec<&str> = vec!["run", "--"];
    args.extend(echo.iter().map(String::as_str));
    let out = daemon.run(&args);
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tty7_e2e_run_marker"), "{stdout}");

    let exit = one_shot("exit 7");
    let mut args: Vec<&str> = vec!["run", "--"];
    args.extend(exit.iter().map(String::as_str));
    let out = daemon.run(&args);
    assert_eq!(
        out.status.code(),
        Some(7),
        "the child's exit code must pass through: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn send_then_capture_round_trip(daemon: &Daemon) {
    let created = daemon.run_json(&["new", &workdir()]);
    let pane = created["pane"].as_u64().expect("new prints the pane id");
    let address = format!("%{pane}");

    daemon.run_ok(&["send", &address, "echo tty7_e2e_capture_marker", "--enter"]);

    let deadline = Instant::now() + SETTLE_WITHIN;
    loop {
        let seen = daemon.run_ok(&["capture", &address, "--scrollback"]);
        if seen.contains("tty7_e2e_capture_marker") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the sent text never showed up in the capture; last capture:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn status_reports_the_live_server(daemon: &Daemon) {
    let status = daemon.run_json(&["status"]);
    assert!(status["pid"].as_u64().is_some_and(|pid| pid > 0), "{status}");
    assert_eq!(
        status["protocol_version"].as_u64(),
        Some(u64::from(PROTOCOL_VERSION)),
        "{status}"
    );

    let human = daemon.run_ok(&["server", "status"]);
    assert!(human.contains("pid"), "{human}");
}

fn events_stream_reports_a_workspace_creation(daemon: &Daemon) {
    let mut watcher = daemon
        .cli(&["events", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start tty7 events");
    let stdout = watcher.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + SETTLE_WITHIN;
    let mut seen = Vec::new();
    let verdict = 'outer: loop {
        if Instant::now() >= deadline {
            break false;
        }
        daemon.run_ok(&["ws", "new", "evtws"]);
        let round = Instant::now() + Duration::from_secs(5);
        while let Ok(line) = rx.recv_timeout(round.saturating_duration_since(Instant::now())) {
            let is_event = serde_json::from_str::<serde_json::Value>(&line).is_ok();
            seen.push(line);
            if is_event {
                break 'outer true;
            }
        }
    };
    let _ = watcher.kill();
    let _ = watcher.wait();
    assert!(
        verdict,
        "no event line arrived within {SETTLE_WITHIN:?}; saw {seen:?}"
    );
}
