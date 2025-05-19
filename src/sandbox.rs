use std::{
    process::{Command, exit},
    sync::Arc,
};

use anyhow::{Context, Result};
use composefs::{
    fsverity::FsVerityHashValue,
    mount::{FsHandle, mount_at},
    mountcompat::prepare_mount,
    repository::Repository,
};
use rustix::fs::CWD;
use rustix::{
    fd::{AsFd, OwnedFd},
    fs::{Mode, OFlags, mkdirat, open, openat, symlinkat},
    io::write,
    mount::{
        FsMountFlags, MountAttrFlags, MountFlags, MountPropagationFlags, OpenTreeFlags,
        UnmountFlags, fsconfig_create, fsmount, mount, open_tree, unmount,
    },
    process::{chdir, pivot_root},
    thread::{UnshareFlags, set_thread_gid, set_thread_uid, unshare},
};

use composefs_fuse::{mount_fuse, open_fuse, serve_tree_fuse};

// ! is still experimental, so let's use this instead.
pub(crate) enum Never {}

fn mount_pseudo(name: &str) -> Result<OwnedFd> {
    let tmpfs = FsHandle::open(name)?;
    fsconfig_create(tmpfs.as_fd())?;
    Ok(fsmount(
        tmpfs.as_fd(),
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::empty(),
    )?)
}

fn mount_tmpfs() -> Result<OwnedFd> {
    mount_pseudo("tmpfs")
}

fn write_to(filename: &str, content: &[u8]) -> Result<()> {
    let fd = open(filename, OFlags::WRONLY, Mode::empty())
        .with_context(|| format!("Failed to open {filename} for writing"))?;

    write(fd, content).with_context(|| format!("Failed to write {content:?} to {filename}"))?;
    Ok(())
}

fn become_root() -> Result<()> {
    unshare(UnshareFlags::NEWUSER).context("Unable to create new user namespace")?;
    unshare(UnshareFlags::NEWNS).context("Unable to create new mount namespace")?;
    //unshare(UnshareFlags::NEWPID).context("Unable to create new pid namespace")?;

    write_to("/proc/self/uid_map", b"0 1000 1\n")?;
    write_to("/proc/self/setgroups", b"deny\n")?;
    write_to("/proc/self/gid_map", b"0 1000 1\n")?;

    set_thread_uid(rustix::fs::Uid::ROOT).context("Unable to setuid(0)")?;
    set_thread_gid(rustix::fs::Gid::ROOT).context("Unable to setgid(0)")?;

    Ok(())
}

fn change_propagation(path: &str, propagation: MountPropagationFlags) -> Result<()> {
    let flags = MountFlags::from_bits_truncate(propagation.bits());
    mount(path, path, "", flags, None)
        .with_context(|| format!("Failed to change propagation of {path} to {propagation:?}"))?;
    Ok(())
}

fn replace_root(newroot: impl AsFd) -> Result<()> {
    /*
    move_mount(
        newroot.as_fd(),
        "",
        CWD,
        "/",
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_BENEATH,
    )?;
    */

    change_propagation("/", MountPropagationFlags::PRIVATE)?;
    mount_at(newroot, CWD, "/tmp")?;
    chdir("/tmp")?;
    pivot_root(".", ".")?;

    unmount("/", UnmountFlags::DETACH)?;

    Ok(())
}

fn bind_mount(fd: impl AsFd) -> rustix::io::Result<OwnedFd> {
    open_tree(
        fd.as_fd(),
        ".",
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_RECURSIVE,
    )
}

fn bind_at(from_dfd: impl AsFd, from_name: &str, to_dfd: impl AsFd, to_name: &str) -> Result<()> {
    mount_at(
        open_tree(
            from_dfd.as_fd(),
            from_name,
            OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        )?,
        to_dfd,
        to_name,
    )?;
    Ok(())
}

fn mkdir(dirfd: impl AsFd, name: &str, mode: u32) -> Result<()> {
    mkdirat(dirfd, name, mode.into()).with_context(|| format!("Failed to mkdir {name:?}"))
}

fn symlink(dirfd: impl AsFd, name: &str, target: &str) -> Result<()> {
    symlinkat(target, dirfd, name)
        .with_context(|| format!("Failed to symlink {name:?} -> {target:?}"))
}

fn touch(dirfd: impl AsFd, name: &str) -> Result<()> {
    openat(
        dirfd,
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        0o444.into(),
    )
    .with_context(|| format!("Unable to create {name:?}"))?;
    Ok(())
}

fn open_path(dirfd: impl AsFd, name: &str, flags: OFlags) -> Result<OwnedFd> {
    let flags = flags | OFlags::PATH | OFlags::CLOEXEC;
    openat(dirfd, name, flags, Mode::empty()).with_context(|| format!("Failed to open {name:?}"))
}

fn open_dir(dirfd: impl AsFd, name: &str) -> Result<OwnedFd> {
    open_path(dirfd, name, OFlags::DIRECTORY)
}

fn populate_dev(root: impl AsFd) -> Result<()> {
    let target = open_dir(&root, "dev")?;
    let source = open_dir(CWD, "/dev")?;

    // TODO: maybe console
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        touch(&target, name)?;
        bind_at(&source, name, &target, name)?;
    }

    symlink(&target, "stdin", "/proc/self/fd/0")?;
    symlink(&target, "stdout", "/proc/self/fd/1")?;
    symlink(&target, "stderr", "/proc/self/fd/2")?;
    symlink(&target, "fd", "/proc/self/fd")?;
    symlink(&target, "ptmx", "pts/ptmx")?;

    mkdir(&target, "pts", 0o777)?;
    // TODO: pts

    mkdir(&target, "shm", 0o777)?;
    mount_at(mount_tmpfs()?, &target, "shm")?;

    Ok(())
}

fn mount_fuse_composefs(
    name: &str,
    repo: &Arc<Repository<impl FsVerityHashValue>>,
) -> Result<OwnedFd> {
    let dev_fuse = open_fuse()?;
    let mnt = mount_fuse(&dev_fuse)?;
    let repo_ = Arc::clone(repo);
    let name = name.to_string();
    std::thread::spawn(move || {
        let filesystem =
            composefs_oci::image::create_filesystem(&repo_, &name, None).expect("bzzt");
        let files = filesystem
            .root
            .get_directory("files".as_ref())
            .expect("no files");
        serve_tree_fuse(dev_fuse, &files, &repo_).expect("bzzt2");
    });

    Ok(mnt)
}

fn create_new_rootfs(
    app: Option<&str>,
    runtime: &str,
    repo: &Arc<Repository<impl FsVerityHashValue>>,
) -> Result<impl AsFd> {
    let root = mount_tmpfs().context("Failed to mount tmpfs for new root filesystem")?;

    // Take this out later.  Only needed for kernels < 6.15.
    let root = prepare_mount(root).context("Failed to temporarily mount new rootfs")?;

    mkdir(&root, "dev", 0o777)?;
    populate_dev(&root).context("Failed to populate /dev")?;

    mkdir(&root, "proc", 0o777)?;
    mount_at(bind_mount(open_dir(CWD, "/proc")?)?, &root, "proc")?;

    mkdir(&root, "sys", 0o777)?;
    mount_at(bind_mount(open_dir(CWD, "/sys")?)?, &root, "sys")?;

    symlink(&root, "bin", "usr/bin")?;
    symlink(&root, "lib", "usr/lib")?;
    symlink(&root, "sbin", "usr/sbin")?;
    symlink(&root, "lib64", "usr/lib64")?;

    mkdir(&root, "usr", 0o777)?;
    mount_at(mount_fuse_composefs(runtime, repo)?, &root, "usr")?;

    mkdir(&root, "etc", 0o777)?;
    mount_at(bind_mount(open_dir(&root, "/usr/etc")?)?, &root, "etc")?;
    bind_at(CWD, "/etc/resolv.conf", &root, "etc/resolv.conf")?;

    if let Some(app) = app {
        mkdir(&root, "app", 0o777)?;
        mount_at(mount_fuse_composefs(app, repo)?, &root, "app")?;
    }

    Ok(root)
}

/// Run the app after the sandbox has been established.
fn run_app(
    app: Option<&str>,
    runtime: &str,
    repo: &Arc<Repository<impl FsVerityHashValue>>,
    command: &str,
    args: &[&str],
) -> Result<Never> {
    become_root()?;

    let rootfs = create_new_rootfs(app, runtime, repo)?;
    replace_root(rootfs)?;

    //let status = Command::new("unshare")
        //.args(["-U", "--map-user=1000", "--map-group=1000", "--"])
    //    .arg(command)
    let status = Command::new(command)
        .args(args)
        .env("FLATPAK_ID", "org.flatpak.test")
        .env("PS1", "[📦 $FLATPAK_ID \\W]\\$ ")
        .status()
        .context("Unable to spawn /bin/sh")?;

    if let Some(code) = status.code() {
        exit(code);
    } else {
        exit(255);
    }
}

pub(crate) fn run_sandboxed(
    app: Option<&str>,
    runtime: &str,
    repo: &Arc<Repository<impl FsVerityHashValue>>,
    command: &str,
    args: &[&str],
) -> ! {
    run_app(app, runtime, repo, command, args).expect("Failed to execute app in sandbox");
    unreachable!(); // sigh
}
