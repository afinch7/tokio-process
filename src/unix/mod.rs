//! Unix handling of child processes
//!
//! Right now the only "fancy" thing about this is how we implement the
//! `Future` implementation on `Child` to get the exit status. Unix offers
//! no way to register a child with epoll, and the only real way to get a
//! notification when a process exits is the SIGCHLD signal.
//!
//! Signal handling in general is *super* hairy and complicated, and it's even
//! more complicated here with the fact that signals are coalesced, so we may
//! not get a SIGCHLD-per-child.
//!
//! Our best approximation here is to check *all spawned processes* for all
//! SIGCHLD signals received. To do that we create a `Signal`, implemented in
//! the `tokio-signal` crate, which is a stream over signals being received.
//!
//! Later when we poll the process's exit status we simply check to see if a
//! SIGCHLD has happened since we last checked, and while that returns "yes" we
//! keep trying.
//!
//! Note that this means that this isn't really scalable, but then again
//! processes in general aren't scalable (e.g. millions) so it shouldn't be that
//! bad in theory...

extern crate libc;
extern crate mio;
extern crate tokio_signal;

mod orphan;
mod reap;

use futures::future::TryFutureExt;
use futures::future::FutureExt;
use futures::stream::StreamExt;
use futures::stream::Stream;
use crate::kill::Kill;
use self::mio::{Poll as MioPoll, PollOpt, Ready, Token};
use self::mio::unix::{EventedFd, UnixReady};
use self::mio::event::Evented;
use self::orphan::{AtomicOrphanQueue, OrphanQueue, Wait};
use self::reap::Reaper;
use self::tokio_signal::unix::Signal;
use self::libc::c_int;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::task::Context;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::process::{self, ExitStatus};
use super::SpawnedChild;
use tokio_reactor::{Handle, PollEvented};

impl Wait for process::Child {
    fn id(&self) -> u32 {
        self.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.try_wait()
    }
}

impl Kill for process::Child {
    fn kill(&mut self) -> io::Result<()> {
        self.kill()
    }
}

lazy_static! {
    static ref ORPHAN_QUEUE: AtomicOrphanQueue<process::Child> = AtomicOrphanQueue::new();
}

struct GlobalOrphanQueue;

impl fmt::Debug for GlobalOrphanQueue {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        ORPHAN_QUEUE.fmt(fmt)
    }
}

impl OrphanQueue<process::Child> for GlobalOrphanQueue {
    fn push_orphan(&self, orphan: process::Child) {
        ORPHAN_QUEUE.push_orphan(orphan)
    }

    fn reap_orphans(&self) {
        ORPHAN_QUEUE.reap_orphans()
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Child {
    inner: Reaper<process::Child, GlobalOrphanQueue, Pin<Box<dyn Stream<Item = io::Result<c_int>> + Send >>>,
}

impl fmt::Debug for Child {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Child")
            .field("pid", &self.inner.id())
            .finish()
    }
}

pub(crate) fn spawn_child(cmd: &mut process::Command, handle: &Handle) -> io::Result<SpawnedChild> {
    let mut child = cmd.spawn()?;
    let stdin = stdio(child.stdin.take(), handle)?;
    let stdout = stdio(child.stdout.take(), handle)?;
    let stderr = stdio(child.stderr.take(), handle)?;

    let signal = Signal::with_handle(libc::SIGCHLD, handle).and_then(|stream| {
        futures::future::ok(stream.map(|res| Ok(res)))
    }).try_flatten_stream().boxed();
    Ok(SpawnedChild {
        child: Child {
            inner: Reaper::new(child, GlobalOrphanQueue, signal),
        },
        stdin,
        stdout,
        stderr,
    })
}

impl Child {
    pub fn id(&self) -> u32 {
        self.inner.id()
    }
}

impl Kill for Child {
    fn kill(&mut self) -> io::Result<()> {
        self.inner.kill()
    }
}

impl Future for Child {
    type Output = io::Result<ExitStatus>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        (&mut Pin::get_mut(self).inner).poll_unpin(cx)
    }
}

#[derive(Debug)]
pub struct Fd<T> 
    where T: AsRawFd + Unpin
{
    inner: T
}

impl<T> io::Read for Fd<T>
    where T: AsRawFd + io::Read + Unpin 
{
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.inner.read(bytes)
    }
}

impl<T> io::Write for Fd<T> 
    where T: AsRawFd + io::Write + Unpin 
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T> AsRawFd for Fd<T> 
    where T: AsRawFd + Unpin
{
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl<T> Evented for Fd<T> 
    where T: AsRawFd + Unpin
{
    fn register(&self,
                poll: &MioPoll,
                token: Token,
                interest: Ready,
                opts: PollOpt)
                -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).register(poll,
                                              token,
                                              interest | UnixReady::hup(),
                                              opts)
    }

    fn reregister(&self,
                  poll: &MioPoll,
                  token: Token,
                  interest: Ready,
                  opts: PollOpt)
                  -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).reregister(poll,
                                                token,
                                                interest | UnixReady::hup(),
                                                opts)
    }

    fn deregister(&self, poll: &MioPoll) -> io::Result<()> {
        EventedFd(&self.as_raw_fd()).deregister(poll)
    }
}

pub type ChildStdin = PollEvented<Fd<process::ChildStdin>>;
pub type ChildStdout = PollEvented<Fd<process::ChildStdout>>;
pub type ChildStderr = PollEvented<Fd<process::ChildStderr>>;

fn stdio<T>(option: Option<T>, handle: &Handle)
            -> io::Result<Option<PollEvented<Fd<T>>>>
    where T: AsRawFd + Unpin
{
    let io = match option {
        Some(io) => io,
        None => return Ok(None),
    };

    // Set the fd to nonblocking before we pass it to the event loop
    unsafe {
        let fd = io.as_raw_fd();
        let r = libc::fcntl(fd, libc::F_GETFL);
        if r == -1 {
            return Err(io::Error::last_os_error())
        }
        let r = libc::fcntl(fd, libc::F_SETFL, r | libc::O_NONBLOCK);
        if r == -1 {
            return Err(io::Error::last_os_error())
        }
    }
    let io = PollEvented::new_with_handle(Fd{ inner: io }, handle)?;
    Ok(Some(io))
}
