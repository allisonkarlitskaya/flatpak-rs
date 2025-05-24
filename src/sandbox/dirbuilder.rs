use std::{
    fs::File,
    io::{BufWriter, Write},
};

use anyhow::{Context, Result};
use rustix::{
    fd::{AsFd, BorrowedFd, OwnedFd},
    fs::{OFlags, mkdirat, openat, symlinkat},
    io::Errno,
    path::Arg as PathArg,
};

use super::{
    mounthandle::MountHandle,
    util::{filter_errno, open_dir},
};

pub(super) struct DirBuilder<'a> {
    dirfd: &'a OwnedFd,
}

impl<'a> DirBuilder<'a> {
    // Note: in case we do a simple uid map, we end up running some prep commands (like ldconfig) as
    // the target uid:gid.  We do this while still holding a full set of capabilities, but the kernel
    // automatically drops capabilities on execve() for non-numerically-0 effective uid.  Create
    // our various directories around the filesystem such that these spawned commands can write to
    // them even without caps: we're going to remount 'ro' before starting the application anyway.
    const DIR_PERMISSION: u32 = 0o755;

    // We don't have the same concerns around files, but let's be consistent.
    const FILE_PERMISSION: u32 = 0o644;

    pub(super) fn new(dirfd: &'a OwnedFd) -> Self {
        Self { dirfd }
    }

    pub(super) fn create_dir(&self, name: &str, mode: u32, exist_ok: bool) -> Result<OwnedFd> {
        let (dirfd, name) = if let Some((parent, name)) = name.rsplit_once('/') {
            (&self.create_dir(parent, mode, true)?, name)
        } else {
            (self.dirfd, name)
        };

        // If exist_ok then optimistically assume that the directory might already exist
        if exist_ok {
            if let Some(dir) = filter_errno(open_dir(dirfd, name), Errno::NOENT)? {
                return Ok(dir);
            }
        }

        // Create the directory
        match mkdirat(dirfd, name, mode.into()) {
            Err(Errno::EXIST) if exist_ok => Ok(()), // recheck this (for races)
            other => other,
        }?;

        Ok(open_dir(dirfd, name)?)
    }

    pub(super) fn create_file(&self, name: &str) -> Result<OwnedFd> {
        let (dirfd, name) = if let Some((parent, name)) = name.rsplit_once('/') {
            (&self.create_dir(parent, Self::DIR_PERMISSION, true)?, name)
        } else {
            (self.dirfd, name)
        };

        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        openat(dirfd, name, flags, Self::FILE_PERMISSION.into())
            .with_context(|| format!("Failed to open {name:?} for writing"))
    }

    pub(super) fn subdir(
        &self,
        name: &str,
        populate: impl Fn(DirBuilder) -> Result<()>,
    ) -> Result<()> {
        populate(DirBuilder {
            dirfd: &self.create_dir(name, Self::DIR_PERMISSION, false)?,
        })
        .with_context(|| format!("Failed to populate subdir {name}"))
    }

    pub(super) fn write(&self, name: &str, content: &str) -> Result<()> {
        Ok(File::from(self.create_file(name)?).write_all(content.as_bytes())?)
    }

    pub(super) fn tee(&self, name: &str) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(File::from(self.create_file(name)?)))
    }

    pub(super) fn tee2(
        &self,
        name: &str,
        populate: impl Fn(BufWriter<File>) -> Result<()>,
    ) -> Result<()> {
        populate(BufWriter::new(File::from(self.create_file(name)?)))
            .with_context(|| format!("Failed to write to file {}", name))
    }

    pub(super) fn symlink(&self, name: &str, target: &str) -> Result<()> {
        symlinkat(target, self.dirfd, name)
            .with_context(|| format!("Failed to symlink {name:?} -> {target:?}"))
    }

    pub(super) fn mount(&self, name: &str, mnt: MountHandle) -> Result<()> {
        mnt.move_to(self.create_dir(name, Self::DIR_PERMISSION, false)?, "")
    }

    pub(super) fn bind_dir(
        &self,
        name: &str,
        from_dirfd: impl AsFd,
        from_name: impl PathArg,
    ) -> Result<()> {
        self.mount(name, MountHandle::clone_recursive(from_dirfd, from_name)?)
    }

    pub(super) fn bind_file(
        &self,
        name: &str,
        from_dirfd: impl AsFd,
        from_name: impl PathArg,
    ) -> Result<()> {
        MountHandle::clone(from_dirfd, from_name)?.move_to(self.create_file(name)?, "")
    }
}

impl<'a> AsFd for DirBuilder<'a> {
    fn as_fd(&self) -> BorrowedFd<'a> {
        self.dirfd.as_fd()
    }
}
