use std::{os::fd::OwnedFd, sync::Arc};
use nix::unistd::Pid;

pub mod netns;

pub fn new_netns(pid:Pid) -> OwnedFd {
    let mut new_netns: Option<OwnedFd> = None;
    std::thread::scope(|s| {
        s.spawn(|| {
            let res = nix::sched::unshare(CloneFlags::CLONE_NEWNET);
            if res.is_err() {
                return;
            }

            if let Ok(newns) =
                std::fs::File::open(format!("/proc/{}/ns/net", pid))
            {
                new_netns = Some(newns.into());
            }
        });
    });

    new_netns.expect("failed to create netns")
}

pub fn new_netns1() -> OwnedFd {
    let mut new_netns: Option<OwnedFd> = None;
    std::thread::scope(|s| {
        s.spawn(|| {
            let res = nix::sched::unshare(CloneFlags::CLONE_NEWNET);
            if res.is_err() {
                return;
            }

            if let Ok(newns) =
                std::fs::File::open(format!("/proc/self/task/{}/ns/net", gettid()))
            {
                new_netns = Some(newns.into());
            }
        });
    });

    new_netns.expect("failed to create netns")
}