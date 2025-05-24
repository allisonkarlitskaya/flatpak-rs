use std::fmt;

use anyhow::{Context, Result};
use rustix::{
    fd::{AsFd, AsRawFd, OwnedFd},
    mount::{
        FsMountFlags, FsOpenFlags, MountAttrFlags, MountPropagationFlags, MoveMountFlags,
        OpenTreeFlags, UnmountFlags, fsconfig_create, fsconfig_set_fd, fsconfig_set_flag,
        fsconfig_set_string, fsmount, fsopen, move_mount, open_tree, unmount,
    },
    path::Arg as PathArg,
    process::{fchdir, pivot_root},
};

use super::mount_setattr::mount_setattr;

// TODO: upstream this back into composefs?
#[derive(Debug)]
pub struct FsHandle {
    fsfd: OwnedFd,
    name: &'static str, // for debug messages
}

#[allow(dead_code)]
impl FsHandle {
    pub fn open(name: &'static str) -> Result<FsHandle> {
        let fsfd = fsopen(name, FsOpenFlags::FSOPEN_CLOEXEC)
            .with_context(|| format!("Failed to fsopen new {name:?}"))?;

        Ok(FsHandle { fsfd, name })
    }

    pub fn set_flag(&self, flag: &str) -> Result<&Self> {
        fsconfig_set_flag(self.fsfd.as_fd(), flag)
            .with_context(|| format!("Failed to set flag {flag:?} on {:?}", self.name))?;
        Ok(self)
    }
    pub fn set_string(&self, key: &str, value: &str) -> Result<&Self> {
        fsconfig_set_string(self.fsfd.as_fd(), key, value)
            .with_context(|| format!("Failed to set {key}={value:?} on {:?}", self.name))?;
        Ok(self)
    }

    pub fn set_fd(&self, key: &str, value: impl AsFd + fmt::Debug) -> Result<&Self> {
        fsconfig_set_fd(self.fsfd.as_fd(), key, value.as_fd())
            .with_context(|| format!("Failed to set {key}={value:?} on {:?}", self.name))?;
        Ok(self)
    }

    pub fn set_int(&self, key: &str, value: u32) -> Result<&Self> {
        self.set_string(key, &format!("{value}"))
    }

    pub fn set_mode(&self, key: &str, value: u16) -> Result<&Self> {
        self.set_string(key, &format!("{value:0o}"))
    }

    pub fn set_fd_str(&self, key: &str, value: impl AsFd) -> Result<&Self> {
        self.set_string(key, &format!("{}", value.as_fd().as_raw_fd()))
    }

    pub fn mount(&self) -> Result<MountHandle> {
        fsconfig_create(self.fsfd.as_fd())?;

        Ok(MountHandle::new(fsmount(
            self.fsfd.as_fd(),
            FsMountFlags::FSMOUNT_CLOEXEC,
            MountAttrFlags::empty(),
        )?))
    }
}

impl Drop for FsHandle {
    fn drop(&mut self) {
        let mut buffer = [0u8; 1024];
        loop {
            match rustix::io::read(&self.fsfd, &mut buffer) {
                Err(_) | Ok(0) => return, // ENODATA, among others?
                Ok(size) => eprintln!(
                    "{:?}: {}",
                    self.name,
                    String::from_utf8(buffer[0..size].to_vec()).unwrap()
                ),
            }
        }
    }
}

pub struct MountHandle {
    pub mountfd: OwnedFd,
}

impl MountHandle {
    pub fn new(mountfd: OwnedFd) -> Self {
        Self { mountfd }
    }

    pub fn clone(dirfd: impl AsFd, path: impl PathArg) -> Result<Self> {
        let flags = OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_EMPTY_PATH;
        Ok(Self::new(open_tree(dirfd.as_fd(), path, flags)?))
    }

    pub fn clone_recursive(dirfd: impl AsFd, path: impl PathArg) -> Result<Self> {
        let flags = OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_RECURSIVE
            | OpenTreeFlags::AT_EMPTY_PATH;
        Ok(Self::new(open_tree(dirfd.as_fd(), path, flags)?))
    }

    pub fn pivot_root(&self) -> Result<()> {
        fchdir(&self.mountfd)?;
        pivot_root(".", ".")?;
        unmount("/", UnmountFlags::DETACH)?;

        Ok(())
    }

    pub fn make_readonly(&self) -> Result<()> {
        mount_setattr(
            &self.mountfd,
            MountAttrFlags::MOUNT_ATTR_RDONLY,
            MountAttrFlags::empty(),
            MountPropagationFlags::empty(),
        )
        .context("Unable to make mount readonly")
    }

    pub fn move_to(&self, dirfd: impl AsFd, name: impl PathArg) -> Result<()> {
        move_mount(
            self.mountfd.as_fd(),
            "",
            dirfd.as_fd(),
            name,
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )?;

        Ok(())
    }
}
