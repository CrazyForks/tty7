//! [`Duplex`] — one bidirectional link, as the *server* side sees it.
//!
//! The client half of a control connection takes its two halves separately
//! ([`ControlClient::connect`](crate::daemon::control::ControlClient::connect)):
//! it is always the side that opened the link, so it already holds whatever it
//! opened. The server is handed things — an accepted socket, or a process it was
//! `exec`'d into — and has to get two halves *out* of them. That is all this
//! trait does.
//!
//! # Why not `try_clone`
//!
//! Every existing server path in the tree splits with `try_clone`
//! (`daemon::server::handle_conn`), which works because a socket is one object
//! with two directions. Under `tty7-server --stdio` it is not: the read half is
//! file descriptor 0 and the write half is file descriptor 1, two unrelated
//! pipes to two different places. There is nothing to clone. So the split
//! happens once, at construction, and the trait says so.
//!
//! # Shutdown is [`LinkShutdown`], not a second abstraction
//!
//! A server thread parked in `read` cannot be woken by a flag — it is inside a
//! syscall, and the peer has no reason to send anything, because it is waiting
//! for a reply. This is precisely the deadlock
//! [`LinkShutdown`](crate::daemon::control::LinkShutdown) exists to break on the
//! client side, and it is the same deadlock here.
//!
//! So [`Halves::shutdown`] **is** a `LinkShutdown`, reusing that trait rather
//! than mirroring it. Two shutdown abstractions that mean the same thing would
//! be two places to forget a transport, and the socket impls would have to be
//! written twice; adopting the existing one costs nothing and keeps a single
//! answer to "how do I force this link to end".
//!
//! It is also non-optional. Every transport a server can be handed *has* an
//! answer — for a socket it is `shutdown(2)`, for stdio it is closing the write
//! half — and making the field an `Option` would only mean the question could be
//! skipped, which is how the client-side deadlock happened in the first place.

use std::io;
use std::sync::{Arc, Mutex};

use crate::daemon::control::LinkShutdown;

/// The two halves of a link, plus the handle that can force the read half to
/// return.
pub struct Halves<R, W> {
    /// Inbound bytes. Read on the connection's own thread.
    pub read: R,
    /// Outbound bytes. Shared by every worker replying on this connection, so
    /// the server keeps it behind a mutex and writes one whole frame per lock.
    pub write: W,
    /// How to end the link from another thread. See the module docs.
    pub shutdown: Arc<dyn LinkShutdown>,
}

/// A bidirectional link a server can serve one connection over.
///
/// Implemented for the accepted-socket types on both platforms and for
/// [`StdioDuplex`]; anything else a future transport brings (an SSH channel,
/// say) implements it the same way.
pub trait Duplex: Send + 'static {
    /// The inbound half.
    type Read: io::Read + Send + 'static;
    /// The outbound half.
    type Write: io::Write + Send + 'static;

    /// Consume the link and yield its halves. Consuming rather than borrowing is
    /// what lets stdio participate: its halves were never one object to begin
    /// with.
    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>>;

    /// A short label for logs and diagnostics, so "the link dropped" can say
    /// *which kind* of link.
    fn kind_label(&self) -> &'static str;
}

#[cfg(unix)]
impl Duplex for std::os::unix::net::UnixStream {
    type Read = std::os::unix::net::UnixStream;
    type Write = std::os::unix::net::UnixStream;

    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>> {
        let read = self.try_clone()?;
        let shutdown: Arc<dyn LinkShutdown> = Arc::new(self.try_clone()?);
        Ok(Halves {
            read,
            write: self,
            shutdown,
        })
    }

    fn kind_label(&self) -> &'static str {
        "unix"
    }
}

impl Duplex for std::net::TcpStream {
    type Read = std::net::TcpStream;
    type Write = std::net::TcpStream;

    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>> {
        let read = self.try_clone()?;
        let shutdown: Arc<dyn LinkShutdown> = Arc::new(self.try_clone()?);
        Ok(Halves {
            read,
            write: self,
            shutdown,
        })
    }

    fn kind_label(&self) -> &'static str {
        "tcp"
    }
}

// ---------------------------------------------------------------------------
// stdio
// ---------------------------------------------------------------------------

/// The process's own stdin and stdout, as one link.
///
/// This is how `tty7-server --stdio --serve` is reached: whatever spawned it
/// (`ssh host tty7-server --stdio`, `wsl.exe -- tty7-server --stdio`, or a test
/// harness) talks to it down the pipes it was born with. There is no socket, no
/// port and no filesystem rendezvous — which is the entire point, because that
/// is what makes the path work under `AllowStreamLocalForwarding no`, under WSL,
/// and in CI on a box with no sshd.
///
/// Unix only. A Windows `tty7-server` is reached over its own loopback
/// transport; nothing in the tree spawns one down a pipe, and the handle
/// surgery below has no portable equivalent worth carrying unused.
#[cfg(unix)]
pub struct StdioDuplex {
    read: std::fs::File,
    write: StdioWriter,
}

#[cfg(unix)]
impl StdioDuplex {
    /// Take exclusive ownership of the process's stdin and stdout.
    ///
    /// **This is a hijack, deliberately.** Both descriptors are duplicated, and
    /// the originals are then pointed at `/dev/null`. After this call `println!`,
    /// a library's stray progress bar, and anything else that reaches for fd 1
    /// write into the void instead of into the middle of a control frame. A
    /// protocol carried on stdout cannot share stdout, and "nothing in this
    /// process ever prints" is not an invariant that survives a dependency
    /// bump — so it is enforced here rather than assumed.
    ///
    /// Diagnostics still work: stderr is untouched, and it is where the stdio
    /// server logs.
    pub fn take() -> io::Result<StdioDuplex> {
        use std::os::fd::FromRawFd as _;

        // Duplicate first, redirect second: if the redirect failed after a
        // successful dup we would still hold a working link, whereas the other
        // order could leave the process with no stdout at all.
        let stdin_fd = dup_fd(libc::STDIN_FILENO)?;
        let stdout_fd = dup_fd(libc::STDOUT_FILENO)?;
        redirect_to_null(libc::STDIN_FILENO)?;
        redirect_to_null(libc::STDOUT_FILENO)?;

        // SAFETY: both fds came from `dup(2)` above, are owned by nobody else,
        // and are handed to `File` exactly once.
        let read = unsafe { std::fs::File::from_raw_fd(stdin_fd) };
        let write = unsafe { std::fs::File::from_raw_fd(stdout_fd) };
        Ok(StdioDuplex {
            read,
            write: StdioWriter {
                inner: Arc::new(Mutex::new(Some(write))),
            },
        })
    }
}

#[cfg(unix)]
impl Duplex for StdioDuplex {
    type Read = std::fs::File;
    type Write = StdioWriter;

    fn split(self) -> io::Result<Halves<Self::Read, Self::Write>> {
        let shutdown: Arc<dyn LinkShutdown> = Arc::new(self.write.clone());
        Ok(Halves {
            read: self.read,
            write: self.write,
            shutdown,
        })
    }

    fn kind_label(&self) -> &'static str {
        "stdio"
    }
}

/// The write half of a [`StdioDuplex`]: a closable stdout.
///
/// Closing it is the whole reason this type exists rather than a bare `File`.
/// The peer of a stdio server has exactly one way to learn the server is
/// finished — reading EOF on the pipe — and the only way to produce that EOF is
/// to drop the last descriptor on our end. So the file lives behind a shared
/// slot that [`LinkShutdown::shutdown_link`] can empty from any thread.
///
/// The read half is deliberately *not* closable the same way. Closing a pipe
/// descriptor another thread is already blocked reading on does not wake it (the
/// open file description outlives the descriptor), so pretending otherwise would
/// be a shutdown that silently does nothing. What actually ends the read is the
/// peer hanging up — which closing our write half is exactly what provokes. It
/// is the same half-close a TCP `shutdown(Write)` performs, for the same reason.
#[cfg(unix)]
#[derive(Clone)]
pub struct StdioWriter {
    inner: Arc<Mutex<Option<std::fs::File>>>,
}

#[cfg(unix)]
impl io::Write for StdioWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(f) => f.write(buf),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdio link was shut down",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(f) => f.flush(),
            // Nothing buffered and nothing to flush it to: the shutdown already
            // happened, and reporting an error here would only turn an orderly
            // close into a logged failure.
            None => Ok(()),
        }
    }
}

#[cfg(unix)]
impl LinkShutdown for StdioWriter {
    fn shutdown_link(&self) -> io::Result<()> {
        let taken = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        if !taken {
            // Already closed. Idempotent on purpose: `close` runs from both the
            // teardown path and a `Drop`, and neither should have to know
            // whether the other went first.
            return Ok(());
        }
        Ok(())
    }
}

#[cfg(unix)]
fn dup_fd(fd: libc::c_int) -> io::Result<libc::c_int> {
    // SAFETY: `fd` is one of the standard descriptors; `dup` either returns a
    // fresh owned descriptor or -1 with errno set.
    let new = unsafe { libc::dup(fd) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(new)
}

#[cfg(unix)]
fn redirect_to_null(fd: libc::c_int) -> io::Result<()> {
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    use std::os::fd::AsRawFd as _;
    // SAFETY: both descriptors are valid and owned here; `dup2` closes `fd`
    // and re-points it at `/dev/null` atomically.
    let rc = unsafe { libc::dup2(null.as_raw_fd(), fd) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// The socket impls hand back halves that really are the same link, and a
    /// shutdown from a third handle ends a parked read — the property the whole
    /// trait exists for.
    #[cfg(unix)]
    #[test]
    fn a_unix_stream_splits_into_working_halves() {
        use std::os::unix::net::UnixStream;
        let (a, b) = UnixStream::pair().unwrap();
        let Halves {
            mut read,
            mut write,
            shutdown,
        } = a.split().unwrap();

        let mut peer = b;
        write.write_all(b"ping").unwrap();
        write.flush().unwrap();
        let mut got = [0u8; 4];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");

        peer.write_all(b"pong").unwrap();
        let mut got = [0u8; 4];
        read.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"pong");

        // The parked reader has to come back, not hang.
        let reader = std::thread::spawn(move || {
            let mut sink = Vec::new();
            read.read_to_end(&mut sink).map(|_| ())
        });
        shutdown.shutdown_link().unwrap();
        let _ = reader.join().unwrap();
    }

    /// Shutting a stdio writer down closes it for good: a later write fails
    /// rather than quietly succeeding into a descriptor the peer no longer has.
    #[cfg(unix)]
    #[test]
    fn a_shut_stdio_writer_refuses_further_writes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let file = std::fs::File::create(tmp.path()).unwrap();
        let mut w = StdioWriter {
            inner: Arc::new(Mutex::new(Some(file))),
        };
        w.write_all(b"before").unwrap();
        w.shutdown_link().unwrap();
        assert_eq!(
            w.write(b"after").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        // Idempotent: teardown and `Drop` both call it.
        w.shutdown_link().unwrap();
        assert_eq!(std::fs::read(tmp.path()).unwrap(), b"before");
    }
}
