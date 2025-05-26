use std::process::Command;

use anyhow::Result;
use rustix::fd::AsFd;
use rustix::io::fcntl_dupfd_cloexec;

use super::{
    argsfd::{ArgsFd, ArgsFdBuilder},
    util::nameat,
    withfds::WithFds,
};

pub(crate) fn dbus_proxy(
    sandbox_dirfd: impl AsFd,
    sandbox_name: &str,
    host_dirfd: impl AsFd,
    host_name: &str,
    flags: &[&str],
) -> Result<()> {
    let host_dirfd = fcntl_dupfd_cloexec(host_dirfd, 0)?;
    let sandbox_dirfd = fcntl_dupfd_cloexec(sandbox_dirfd, 0)?;

    let args = ArgsFdBuilder::new()?;
    args.add(format!("unix:path={}", nameat(&host_dirfd, host_name)))?;
    args.add(nameat(&sandbox_dirfd, sandbox_name))?;
    args.add("--log")?;
    args.extend(flags)?;
    let args_fd = args.done();

    Command::new("xdg-dbus-proxy")
        .arg(args_fd.as_arg())
        .with_fds([host_dirfd, sandbox_dirfd, args_fd])
        .spawn()?;

    Ok(())
}
