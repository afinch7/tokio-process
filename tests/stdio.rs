#![feature(async_await)]

extern crate futures;
#[macro_use]
extern crate log;
extern crate tokio_io;
extern crate tokio_process;

use std::io::{self, BufReader};
use std::process::{Stdio, ExitStatus, Command};
use std::future::Future;
use std::task::Poll;
use std::task::Context;
use std::pin::Pin;

use futures::future::FutureExt;
use futures::stream::TryStreamExt;
use futures::io::AsyncBufReadExt;
use futures::io::AsyncRead;
use futures::io::AsyncReadExt;
use futures::io::AsyncWriteExt;
use futures::stream::{self, StreamExt};
use tokio_process::{CommandExt, Child};

mod support;

fn cat() -> Command {
    let mut cmd = support::cmd("cat");
    cmd.stdin(Stdio::piped())
       .stdout(Stdio::piped());
    cmd
}

fn feed_cat(mut cat: Child, n: usize) -> Pin<Box<dyn Future<Output = io::Result<ExitStatus>>>> {
    let stdin = cat.stdin().take().unwrap();
    let stdout = cat.stdout().take().unwrap();

    debug!("starting to feed");
    // Produce n lines on the child's stdout.
    let numbers = stream::iter(0..n);
    let write = numbers.fold(stdin, async move |mut stdin, i| {
        debug!("sending line {} to child", i);
        let bytes = format!("line {}\n", i).into_bytes();
        AsyncWriteExt::write_all(&mut stdin, &bytes).await.unwrap();
        stdin
    }).map(|_| ());

    // Try to read `n + 1` lines, ensuring the last one is empty
    // (i.e. EOF is reached after `n` lines.
    let reader = futures::io::BufReader::new(stdout);
    let expected_numbers = stream::iter(0..=n);
    let read = expected_numbers.fold((reader, 0), async move |(mut reader, i), _| {
        let done = i >= n;
        debug!("starting read from child");
        let mut vec = Vec::new();
        AsyncBufReadExt::read_until(&mut reader, b'\n', &mut vec).await.unwrap();
        debug!("read line {} from child ({} bytes, done: {})",
            i, vec.len(), done);
        match (done, vec.len()) {
            (false, 0) => {
                 panic!("broken pipe");
            },
            (true, n) if n != 0 => {
                 panic!("extraneous data");
            },
            _ => {
                let s = std::str::from_utf8(&vec).unwrap();
                let expected = format!("line {}\n", i);
                if done || s == expected {
                    (reader, i + 1)
                } else {
                    panic!("unexpected data");
                }
            }
        }
    });

    // Compose reading and writing concurrently.
    futures::future::join(write, read).then(|_| {
        cat
    }).boxed()
}

/// Check for the following properties when feeding stdin and
/// consuming stdout of a cat-like process:
///
/// - A number of lines that amounts to a number of bytes exceeding a
///   typical OS buffer size can be fed to the child without
///   deadlock. This tests that we also consume the stdout
///   concurrently; otherwise this would deadlock.
///
/// - We read the same lines from the child that we fed it.
///
/// - The child does produce EOF on stdout after the last line.
#[test]
fn feed_a_lot() {
    let child = cat().spawn_async().unwrap();
    let status = support::run_with_timeout(feed_cat(child, 10000)).unwrap();
    assert_eq!(status.code(), Some(0));
}

// FIXME: delete this test once we have a resolution for #51
// This test's setup is flaky, and setting up a consistent test is nearly
// impossible: right now we invoke `cat` and immediately kill it, expecting
// that it didn't write anything, but if there's something wrong with the
// command itself (e.g. redirection issues, it doesn't actually print anything
// out, etc.) this test can falsely pass. Attempting a solution which writes
// some data, *then* kill the child, write more data, and assert that only the
// first write is echoed back seems like a good approach, however, due to the
// ordering of context switches or how the kernel buffers data we can get
// inconsistent results. We can keep this test around for now, but as soon as
// we have a solution for #51, we may have a better avenue for testing this
// functionality.
#[test]
fn drop_kills() {
    let mut child = cat().spawn_async().unwrap();
    let mut stdout = child.stdout().take().unwrap();

    let mut output = Vec::new();

    // Ignore all write errors since we expect a broken pipe here
    let writer = async {
        let mut stdin = child.stdin().take().unwrap();
        AsyncWriteExt::write_all(&mut stdin, b"1234").await;
        drop(child);
    };
    let reader = AsyncReadExt::read_to_end(&mut stdout, &mut output);

    support::CurrentThreadRuntime::new()
        .expect("failed to get rt")
        .spawn(writer)
        .block_on(support::with_timeout(reader))
        .expect("failed to get output");

    assert_eq!(output.len(), 0);
}

#[test]
fn wait_with_output_captures() {
    let mut child = cat().spawn_async().unwrap();
    let mut stdin = child.stdin().take().unwrap();

    let write_bytes = b"1234";

    let future = async {
        AsyncWriteExt::write_all(&mut stdin, write_bytes).await?;
        dbg!("WRITE COMPLETE");
        let mut output = Vec::new();
        let mut stdout = child.stdout().take().unwrap();
        AsyncReadExt::read_to_end(&mut stdout, &mut output).await?;
        dbg!("READ COMPLETE", output);
        let out = child.wait_with_output();
        out.await
    };

    let ret = support::run_with_timeout(future).unwrap();
    let output = ret;

    assert!(output.status.success());
    assert_eq!(output.stdout, write_bytes);
    assert_eq!(output.stderr.len(), 0);
}

#[test]
fn status_closes_any_pipes() {
    // Cat will open a pipe between the parent and child.
    // If `status_async` doesn't ensure the handles are closed,
    // we would end up blocking forever (and time out).
    let child = cat().status_async().expect("failed to spawn child");

    support::run_with_timeout(child)
        .expect("time out exceeded! did we get stuck waiting on the child?");
}
