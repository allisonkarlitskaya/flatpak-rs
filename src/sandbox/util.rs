use anyhow::{Context, Result};
use rustix::{
    fd::{AsFd, AsRawFd, OwnedFd},
    fs::{CWD, Mode, OFlags, open, openat},
    io::{Errno, write},
    path::Arg as PathArg,
};

/// Writes the string to a given filename.  Really only suitable for stuff in /sys or /proc.
pub(super) fn write_to(filename: &str, content: &str) -> Result<()> {
    let fd = open(filename, OFlags::WRONLY, Mode::empty())
        .with_context(|| format!("Failed to open {filename} for writing"))?;

    write(fd, content.as_bytes())
        .with_context(|| format!("Failed to write {content:?} to {filename}"))?;
    Ok(())
}

/// Opens a file with O_PATH plus the given flags.  Always sets CLOEXEC.
pub(super) fn open_path(
    dirfd: impl AsFd,
    name: impl PathArg,
    flags: OFlags,
) -> rustix::io::Result<OwnedFd> {
    let flags = flags | OFlags::PATH | OFlags::CLOEXEC;
    openat(dirfd, name, flags, Mode::empty())
}

/// Opens a directory with O_PATH.  Always sets CLOEXEC.
pub(super) fn open_dir(dirfd: impl AsFd, name: impl PathArg) -> rustix::io::Result<OwnedFd> {
    open_path(dirfd, name, OFlags::DIRECTORY)
}

/// Turns one particular errno into Ok(None).  Useful for NOENT, EXIST, NOTTY, etc.
pub(super) fn filter_errno<T>(
    result: rustix::io::Result<T>,
    ignored: Errno,
) -> rustix::io::Result<Option<T>> {
    match result {
        Ok(result) => Ok(Some(result)),
        Err(err) if err == ignored => Ok(None),
        Err(err) => Err(err),
    }
}

/// Turns a (dirfd, name) pair into a filename for use with syscalls that don't have an _at()
/// variant, such as the socket API.  Works like AT_EMPTY_PATH: name == "" uses the fd itself.
pub(super) fn nameat(dirfd: impl AsFd, name: &str) -> String {
    let fd = dirfd.as_fd().as_raw_fd();

    if fd == CWD.as_raw_fd() || name.starts_with('/') {
        name.to_string()
    } else if name.is_empty() {
        format!("/proc/self/fd/{fd}")
    } else {
        format!("/proc/self/fd/{fd}/{name}")
    }
}
