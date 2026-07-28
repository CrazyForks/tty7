//! The `tty7-server` command line: the three subcommands, and the two shapes
//! `--stdio` takes.
//!
//! `stdio_conformance.rs` proves the *protocol* over `--stdio --serve`. This
//! file proves the argument handling and the byte bridge — the mode that carries
//! a connection to a control server that is already running, which is the path
//! an `ssh host tty7-server --stdio` takes on a machine with a live daemon and
//! which no amount of `Host` conformance would exercise.

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use tty7_core::daemon::control::{ControlHello, LinkShutdown};
use tty7_core::host::Host;
use tty7_core::host::local::LocalHost;
use tty7_core::host::remote::RemoteHost;
use tty7_core::host::server;

const EXE: &str = env!("CARGO_BIN_EXE_tty7-server");

struct ServerProcess(Mutex<Option<Child>>);

impl LinkShutdown for ServerProcess {
    fn shutdown_link(&self) -> io::Result<()> {
        if let Some(mut c) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        Ok(())
    }
}

/// Start `tty7-server --stdio <args>` and connect a `RemoteHost` to its pipes.
fn stdio_child(args: &[&str]) -> io::Result<Arc<RemoteHost>> {
    let mut child = Command::new(EXE)
        .arg("--stdio")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let out = child.stdout.take().expect("piped");
    let inp = child.stdin.take().expect("piped");
    let closer: Arc<dyn LinkShutdown> = Arc::new(ServerProcess(Mutex::new(Some(child))));
    let hello = ControlHello::host_rpc("cli-test", "localhost");
    RemoteHost::connect_with(out, inp, Some(closer), "stdio:cli", &hello)
}

/// A control server on a temp socket, for the bridge to reach.
fn listening_server(dir: &tempfile::TempDir) -> PathBuf {
    let sock = dir.path().join("control.sock");
    let listener = server::bind_control_socket(&sock).unwrap();
    std::thread::spawn(move || server::serve_listener(listener, LocalHost::new()));
    sock
}

/// **The bridge.** `--stdio --bridge` forwards bytes between its own pipes and a
/// control server that is already listening, parsing nothing on the way.
///
/// That "parsing nothing" is the load-bearing part: the version handshake this
/// stream carries belongs to the client and the server at the far end, and a
/// bridge with an opinion about the protocol would become a third party to a
/// negotiation it is not qualified to join.
#[test]
fn the_bridge_carries_a_whole_session() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock = listening_server(&dir);

    let host = stdio_child(&["--bridge", "--control-sock", &sock.to_string_lossy()])
        .expect("bridge handshake");

    let sandbox = tempfile::TempDir::new().unwrap();
    let f = host.join(sandbox.path(), "through-the-bridge.txt");
    host.write_file(&f, b"two hops").unwrap();
    assert_eq!(host.read_file(&f, 1024).unwrap(), b"two hops");

    // A payload big enough that it cannot arrive in one read, so the bridge's
    // copy loop is doing real work rather than passing a single buffer through.
    let big = host.join(sandbox.path(), "big.bin");
    let body: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    host.write_file(&big, &body).unwrap();
    assert!(host.read_file(&big, 8 * 1024 * 1024).unwrap() == body);

    // Out-of-order replies survive the extra hop too: the bridge must not
    // serialize what the server took care to keep concurrent.
    let entries = host.read_dir(sandbox.path(), None).unwrap();
    assert_eq!(entries.len(), 2);
}

/// `--bridge` with nowhere to bridge to fails rather than quietly serving
/// itself. An operator who asked for the bridge is telling us a server exists;
/// silently becoming that server would fork the machine's state in two.
#[test]
fn an_explicit_bridge_with_no_server_fails() {
    let dir = tempfile::TempDir::new().unwrap();
    let missing = dir.path().join("nobody-here.sock");
    let err = stdio_child(&["--bridge", "--control-sock", &missing.to_string_lossy()]);
    assert!(
        err.is_err(),
        "--bridge should not fall back to serving in-process"
    );
}

/// With neither flag, `--stdio` probes: nothing listening means serve here, so a
/// machine that has never run a daemon is still reachable over ssh.
#[test]
fn the_default_mode_serves_when_nothing_is_listening() {
    let dir = tempfile::TempDir::new().unwrap();
    let missing = dir.path().join("nobody-here.sock");
    let host = stdio_child(&["--control-sock", &missing.to_string_lossy()])
        .expect("the probe should fall through to serving");
    let sandbox = tempfile::TempDir::new().unwrap();
    assert!(host.exists(sandbox.path()));
}

/// ...and something listening means bridge to it, so a second `--stdio` session
/// joins the machine's existing server instead of standing up a rival.
#[test]
fn the_default_mode_bridges_when_a_server_is_listening() {
    let dir = tempfile::TempDir::new().unwrap();
    let sock = listening_server(&dir);
    let host =
        stdio_child(&["--control-sock", &sock.to_string_lossy()]).expect("the probe should bridge");
    let sandbox = tempfile::TempDir::new().unwrap();
    let f = host.join(sandbox.path(), "auto.txt");
    host.write_file(&f, b"ok").unwrap();
    assert_eq!(std::fs::read(&f).unwrap(), b"ok");
}

/// Contradictory flags are refused rather than one silently winning.
#[test]
fn serve_and_bridge_together_are_refused() {
    let out = Command::new(EXE)
        .args(["--stdio", "--serve", "--bridge"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("opposite"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `agent-hook` runs the same emitter the GUI binary does, and stays quiet.
///
/// Quiet is the requirement, not a nicety: this runs as a child of an agent's
/// hook runner, and anything it prints lands in the agent's own transcript. With
/// no controlling terminal there is nowhere to emit to, and it still has to
/// succeed — a hook that fails is a hook the agent reports as broken.
#[test]
fn agent_hook_is_quiet_and_succeeds() {
    let out = Command::new(EXE)
        .args(["agent-hook", "claude", "Stop"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "agent-hook exited {:?}", out.status);
    assert!(out.stdout.is_empty(), "agent-hook wrote to stdout");
}

/// A malformed `agent-hook` invocation is still silent and still succeeds — the
/// emitter's whole contract is that it never becomes the agent's problem.
#[test]
fn agent_hook_without_arguments_still_succeeds() {
    let out = Command::new(EXE)
        .arg("agent-hook")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn version_and_help_report_on_stdout() {
    let v = Command::new(EXE).arg("--version").output().unwrap();
    assert!(v.status.success());
    assert!(String::from_utf8_lossy(&v.stdout).starts_with("tty7-server "));

    let h = Command::new(EXE).arg("--help").output().unwrap();
    assert!(h.status.success());
    let text = String::from_utf8_lossy(&h.stdout);
    for expected in ["--daemon", "--stdio", "agent-hook", "--control-sock"] {
        assert!(text.contains(expected), "help omits {expected}: {text}");
    }
}

/// No arguments is a usage error, not a process that sits there doing nothing.
#[test]
fn no_arguments_is_a_usage_error() {
    let out = Command::new(EXE).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--daemon"));
}
