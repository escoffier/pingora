// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use nix::sched::{setns, CloneFlags};
use std::os::fd::OwnedFd;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;
use log::info;

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub struct NetnsID {
    pub inode: libc::ino_t,
    pub dev: libc::dev_t,
}

// This is similar to netns_rs, but because we know we always have the same netns to revert to,
// we can make it more efficient with less chances of errors.

#[derive(Clone, Debug)]
pub struct InpodNetns {
    inner: Arc<NetnsInner>,
}
#[derive(Debug)]
struct NetnsInner {
    cur_netns: Arc<OwnedFd>,
    netns: OwnedFd,
    netns_id: NetnsID,
}

impl InpodNetns {
    pub fn current() -> std::io::Result<OwnedFd> {
        let curns =
            std::fs::File::open(format!("/proc/self/task/{}/ns/net", nix::unistd::gettid()));
        curns.map(|f| f.into())
    }

    pub fn capable() -> std::io::Result<()> {
        // set the netns to our current netns. This is intended to be a no-op,
        // and meant to be used as a test, so we can fail early if we can't set the netns
        let curns = Self::current()?;
        setns(curns.as_raw_fd(), CloneFlags::CLONE_NEWNET)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
    }

    pub fn new(cur_netns: Arc<OwnedFd>, workload_netns: OwnedFd) -> std::io::Result<Self> {
        let res = nix::sys::stat::fstat(workload_netns.as_raw_fd())
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        let inode = res.st_ino;
        let dev = res.st_dev;
        Ok(InpodNetns {
            inner: Arc::new(NetnsInner {
                cur_netns,
                netns: workload_netns,
                netns_id: NetnsID { inode, dev },
            }),
        })
    }
    pub fn workload_netns(&self) -> std::os::fd::BorrowedFd {
        use std::os::fd::AsFd;
        self.inner.netns.as_fd()
    }

    // useful for logging / debugging
    pub fn workload_netns_id(&self) -> NetnsID {
        self.inner.netns_id
    }

    pub fn run<F, T>(&self, f: F) -> std::io::Result<T>
    where
        F: FnOnce() -> T,
    {
        setns(self.inner.netns.as_raw_fd(), CloneFlags::CLONE_NEWNET)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        info!("netns: {}", self.inner.netns.as_raw_fd());
        let ret = f();
        setns(self.inner.cur_netns.as_raw_fd(), CloneFlags::CLONE_NEWNET).expect("this must never fail");
        info!("cur_netns: {}", self.inner.cur_netns.as_raw_fd());
        Ok(ret)
    }
}

impl std::os::unix::io::AsRawFd for InpodNetns {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.inner.netns.as_raw_fd()
    }
}

impl PartialEq for InpodNetns {
    fn eq(&self, other: &Self) -> bool {
        // Two netnses can be considered the same if the ino and dev they point to are the same
        // (see - cilium, vishvananda/netns, others)
        self.inner.netns_id == other.inner.netns_id
    }
}

impl Eq for InpodNetns {}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sched::unshare;
    use nix::unistd::gettid;
    use std::assert;
    use std::os::fd::OwnedFd;
    use std::process::Command;

    fn new_netns() -> OwnedFd {
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

    pub fn can_run_privilged_test() -> bool {
        let is_root = unsafe { libc::getuid() } == 0;
        if !is_root && std::env::var("CI").is_ok() {
            panic!("CI tests should run as root to have full coverage");
        }
        is_root
    }

    pub fn run_command(cmd: &str) -> anyhow::Result<()> {
        let now = Instant::now();
        debug!("running command {cmd}");
        let output = Command::new("sh").arg("-c").arg(cmd).output()?;
        debug!(
            "command complete in {:?}; code={}, stdout={}, stderr={}",
            now.elapsed(),
            output.status,
            std::str::from_utf8(&output.stdout)?,
            std::str::from_utf8(&output.stderr)?
        );
        if !output.status.success() {
            anyhow::bail!(
                "command {} exited with code={}, stdout={}, stderr={}",
                cmd.chars().take(50).collect::<String>(),
                output.status,
                std::str::from_utf8(&output.stdout)?,
                std::str::from_utf8(&output.stderr)?
            );
        }
        Ok(())
    }

    #[test]
    fn test_run_works() {
        if !can_run_privilged_test() {
            eprintln!("This test requires root; skipping");
            return;
        }

        // start with new netns to not impact the current netns
        unshare(CloneFlags::CLONE_NEWNET).unwrap();
        let cur_netns = InpodNetns::current().unwrap();
        run_command("ip link add name dummy1 type dummy").unwrap();

        let other_netns = new_netns();

        let sync_netns =
            netns_rs::get_from_path(format!("/proc/self/fd/{}", other_netns.as_raw_fd())).unwrap();
        sync_netns
            .run(|_| run_command("ip link add name dummy2 type dummy"))
            .expect("netns run failed")
            .unwrap();

        // test with future netns
        let netns = InpodNetns::new(Arc::new(cur_netns), other_netns).unwrap();

        let output = netns
            .run(|| Command::new("ip").args(["link", "show"]).output())
            .expect("netns run failed")
            .expect("tokio command failed");

        assert!(output.status.success());
        let out_str = String::from_utf8_lossy(output.stdout.as_slice());
        assert!(!out_str.contains("dummy1"));
        assert!(out_str.contains("dummy2"));

        // make sure we returned to the original ns

        let output = Command::new("ip").args(["link", "show"]).output().unwrap();

        assert!(output.status.success());
        let out_str = String::from_utf8_lossy(output.stdout.as_slice());
        assert!(out_str.contains("dummy1"));
        assert!(!out_str.contains("dummy2"));
    }
}
