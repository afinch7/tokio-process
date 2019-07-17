extern crate futures;
extern crate tokio;

use futures::future::FutureExt;
use tokio::timer::Timeout;
use std::env;
use std::process::Command;
use std::time::Duration;
use std::future::Future;

pub use self::tokio::runtime::current_thread::Runtime as CurrentThreadRuntime;

pub fn cmd(s: &str) -> Command {
    let mut me = env::current_exe().unwrap();
    me.pop();
    if me.ends_with("deps") {
        me.pop();
    }
    me.push(s);
    Command::new(me)
}

pub fn with_timeout<F: Future>(future: F) -> impl Future<Output = F::Output> {
    Timeout::new(future, Duration::from_secs(10)).then(|r| {
        if r.is_err() {
            panic!("timed out {:?}", r.err());
        }
        futures::future::ready(r.unwrap())
    })
}

pub fn run_with_timeout<F>(future: F) -> F::Output
where
    F: Future,
{
    // NB: Timeout requires a timer registration which is provided by
    // tokio's `current_thread::Runtime`, but isn't available by just using
    // tokio's default CurrentThread executor which powers `current_thread::block_on_all`.
    let mut rt = CurrentThreadRuntime::new().expect("failed to get runtime");
    rt.block_on(with_timeout(future))
}
