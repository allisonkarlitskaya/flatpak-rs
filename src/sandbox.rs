use std::{
    collections::HashSet,
    fmt,
    fs::File,
    io::{BufWriter, Write as IoWrite},
    path::Path,
    process::{Command, exit},
    sync::Arc,
};

use anyhow::{Context, Result};
use composefs::{fsverity::FsVerityHashValue, repository::Repository};
use rustix::{
    fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    fs::{CWD, Gid, Mode, OFlags, Uid, fchown, mkdirat, open, openat, symlinkat},
    io::{Errno, dup, write},
    mount::{
        FsMountFlags, FsOpenFlags, MountAttrFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
        fsconfig_create, fsconfig_set_fd, fsconfig_set_flag, fsconfig_set_string, fsmount, fsopen,
        move_mount, open_tree, unmount,
    },
    path::Arg as PathArg,
    process::{fchdir, getgid, getuid, pivot_root},
    termios::ttyname,
    thread::{UnshareFlags, set_thread_gid, set_thread_uid, unshare},
};

use composefs_fuse::{open_fuse, serve_tree_fuse};

// TODO: upstream this back into composefs
pub struct FsHandle {
    fsfd: OwnedFd,
    name: &'static str, // for debug messages
}

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
    mountfd: OwnedFd,
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

    pub fn pivot_root(self) -> Result<()> {
        fchdir(&self.mountfd)?;
        pivot_root(".", ".")?;
        unmount("/", UnmountFlags::DETACH)?;

        Ok(())
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

// ! is still experimental, so let's use this instead.
pub(crate) enum Never {}

fn mount_tmpfs(name: &str) -> Result<MountHandle> {
    FsHandle::open("tmpfs")?.set_string("source", name)?.mount()
}

fn mount_devpts() -> Result<MountHandle> {
    FsHandle::open("devpts")?
        .set_flag("newinstance")?
        .set_mode("ptmxmode", 0o666)?
        .set_mode("mode", 0o620)?
        .mount()
}

fn write_to(filename: &str, content: &str) -> Result<()> {
    let fd = open(filename, OFlags::WRONLY, Mode::empty())
        .with_context(|| format!("Failed to open {filename} for writing"))?;

    write(fd, content.as_bytes())
        .with_context(|| format!("Failed to write {content:?} to {filename}"))?;
    Ok(())
}

fn open_path(dirfd: impl AsFd, name: impl PathArg, flags: OFlags) -> rustix::io::Result<OwnedFd> {
    let flags = flags | OFlags::PATH | OFlags::CLOEXEC;
    openat(dirfd, name, flags, Mode::empty())
}

fn open_dir(dirfd: impl AsFd, name: impl PathArg) -> rustix::io::Result<OwnedFd> {
    open_path(dirfd, name, OFlags::DIRECTORY)
}

fn mount_fuse_composefs(
    name: &str,
    repo: &Arc<Repository<impl FsVerityHashValue>>,
) -> Result<MountHandle> {
    let dev_fuse = open_fuse()?;

    // Create the mount
    let mount = FsHandle::open("fuse")?
        .set_flag("ro")?
        .set_flag("default_permissions")?
        .set_flag("allow_other")?
        .set_string("source", "composefs-fuse")?
        .set_fd_str("fd", &dev_fuse)?
        .set_mode("rootmode", 0o40555)?
        .set_int("user_id", getuid().as_raw())?
        .set_int("group_id", getgid().as_raw())?
        .mount()?;

    // Spawn the server thread
    let repo = Arc::clone(repo);
    let name = name.to_string();
    std::thread::spawn(move || {
        let filesystem = composefs_oci::image::create_filesystem(&repo, &name, None).expect("bzzt");
        let files = filesystem
            .root
            .get_directory("files".as_ref())
            .expect("no files");
        serve_tree_fuse(dev_fuse, files, &repo).expect("bzzt2");
    });

    Ok(mount)
}

fn bind_controlling_terminal() -> Result<Option<MountHandle>> {
    // This is all a bit more complicated than it should be.  We need to find the original name of
    // the controlling terminal so that we can reopen it from inside of the current mount
    // namespace (which is required for creating a bind mount).  We also can't use /dev/tty because
    // then ttyname() will just tell us "/dev/tty".
    filter_errno(ttyname(std::io::stdout(), []), Errno::NOTTY)
        .context("Unable to determine name of controlling terminal")?
        .map(|name| {
            // We need to reopen the file in the current namespace in order to be able to clone it
            MountHandle::clone(CWD, &name)
                .with_context(|| format!("Failed to reopen controlling terminal device {name:?}"))
        })
        .transpose()
}

struct DirBuilder {
    dirfd: OwnedFd,
}

impl AsFd for DirBuilder {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.dirfd.as_fd()
    }
}

fn filter_errno<T>(result: rustix::io::Result<T>, ignored: Errno) -> rustix::io::Result<Option<T>> {
    match result {
        Ok(result) => Ok(Some(result)),
        Err(err) if err == ignored => Ok(None),
        Err(err) => Err(err),
    }
}

impl DirBuilder {
    fn new(dirfd: OwnedFd) -> Self {
        Self { dirfd }
    }

    fn create_dir(&self, name: &str, mode: u32, exist_ok: bool) -> Result<OwnedFd> {
        let (dirfd, name) = if let Some((parent, name)) = name.rsplit_once('/') {
            (&self.create_dir(parent, mode, true)?, name)
        } else {
            (&self.dirfd, name)
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

    fn mkdir(&self, name: &str, mode: u32) -> Result<()> {
        let (dirfd, name) = if let Some((parent, name)) = name.rsplit_once('/') {
            (&self.create_dir(parent, mode, true)?, name)
        } else {
            (&self.dirfd, name)
        };

        mkdirat(dirfd, name, mode.into())
            .with_context(|| format!("Unable to create directory {name}"))
    }

    fn create_file(&self, name: &str) -> Result<OwnedFd> {
        let (dirfd, name) = if let Some((parent, name)) = name.rsplit_once('/') {
            (&self.create_dir(parent, 0o755, true)?, name)
        } else {
            (&self.dirfd, name)
        };

        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        openat(dirfd, name, flags, 0o644.into())
            .with_context(|| format!("Failed to open {name:?} for writing"))
    }

    fn subdir(&self, name: &str, populate: impl Fn(DirBuilder) -> Result<()>) -> Result<()> {
        populate(DirBuilder {
            dirfd: self.create_dir(name, 0o755, false)?,
        })
        .with_context(|| format!("Failed to populate subdir {name}"))
    }

    fn write(&self, name: &str, content: &str) -> Result<()> {
        Ok(File::from(self.create_file(name)?).write_all(content.as_bytes())?)
    }

    fn tee(&self, name: &str) -> Result<BufWriter<File>> {
        Ok(BufWriter::new(File::from(self.create_file(name)?)))
    }

    fn tee2(&self, name: &str, populate: impl Fn(BufWriter<File>) -> Result<()>) -> Result<()> {
        populate(BufWriter::new(File::from(self.create_file(name)?)))
            .with_context(|| format!("Failed to write to file {}", name))
    }

    fn symlink(&self, name: &str, target: &str) -> Result<()> {
        symlinkat(target, &self.dirfd, name)
            .with_context(|| format!("Failed to symlink {name:?} -> {target:?}"))
    }

    fn mount(&self, name: &str, mnt: MountHandle) -> Result<()> {
        mnt.move_to(self.create_dir(name, 0o755, false)?, "")
    }

    fn bind_dir(&self, name: &str, from_dirfd: impl AsFd, from_name: impl PathArg) -> Result<()> {
        self.mount(name, MountHandle::clone_recursive(from_dirfd, from_name)?)
    }

    fn bind_file(&self, name: &str, from_dirfd: impl AsFd, from_name: impl PathArg) -> Result<()> {
        MountHandle::clone(from_dirfd, from_name)?.move_to(self.create_file(name)?, "")
    }
}

struct SandboxConfig<ObjectID: FsVerityHashValue> {
    repo: Arc<Repository<ObjectID>>,
    app: Option<String>,
    runtime: String,

    username: String,
    groupname: String,
    gecos: String,
    uid: Uid,
    gid: Gid,
    home: String,

    flags: HashSet<String>,
}

impl<ObjectID: FsVerityHashValue> SandboxConfig<ObjectID> {
    fn unshare(&self) -> Result<()> {
        let outside_uid = getuid().as_raw();
        let outside_gid = getgid().as_raw();

        unshare(UnshareFlags::NEWUSER).context("Unable to create new user namespace")?;

        let uid = self.uid.as_raw();
        let gid = self.gid.as_raw();

        write_to("/proc/self/uid_map", &format!("{uid} {outside_uid} 1\n"))?;
        write_to("/proc/self/setgroups", "deny\n")?;
        write_to("/proc/self/gid_map", &format!("{gid} {outside_gid} 1\n"))?;

        // NB: we're definitely single-threaded at the moment (since unshare(NEWUSER) succeeded)

        //set_thread_uid(Uid::ROOT).context("Unable to setuid(0)")?;
        //set_thread_gid(Gid::ROOT).context("Unable to setgid(0)")?;

        // TODO: figure out how to unset this without getting EPERM
        // set_thread_groups(&[Gid::ROOT]).context("Unable to drop supplementary groups")?;

        unshare(UnshareFlags::NEWNS).context("Unable to create new mount namespace")?;

        Ok(())
    }

    fn drop_capabilities(&self) -> Result<()> {
        set_thread_uid(self.uid).with_context(|| format!("Unable to setuid({:?})", self.uid))?;
        set_thread_gid(self.gid).with_context(|| format!("Unable to setgid({:?})", self.gid))?;
        Ok(())
    }

    fn populate_dev(&self, dev: DirBuilder) -> Result<()> {
        let host_dev = open_dir(CWD, "/dev")?;
        for name in ["full", "null", "random", "tty", "urandom", "zero"] {
            dev.bind_file(name, &host_dev, name)?;
        }

        if let Some(console) = bind_controlling_terminal()? {
            console.move_to(dev.create_file("console")?, "")?;
        }

        dev.symlink("stdin", "/proc/self/fd/0")?;
        dev.symlink("stdout", "/proc/self/fd/1")?;
        dev.symlink("stderr", "/proc/self/fd/2")?;
        dev.symlink("fd", "/proc/self/fd")?;
        dev.symlink("ptmx", "pts/ptmx")?;

        dev.mount("pts", mount_devpts()?)?;
        dev.mount("shm", mount_tmpfs("shm")?)?;

        Ok(())
    }

    fn populate_etc(&self, etc: DirBuilder) -> Result<()> {
        let host_etc = open_dir(CWD, "/etc")?;
        for name in ["resolv.conf", "localtime"] {
            etc.bind_file(name, &host_etc, name)?;
        }

        let username = &self.username;
        let groupname = &self.groupname;
        let uid = self.uid.as_raw();
        let gid = self.gid.as_raw();
        let gecos = &self.gecos;
        let home = &self.home;

        // tee2() has better error reporting and manages the fp itself
        etc.tee2("passwd", |mut fp| {
            writeln!(fp, "root:x:0:0:root:/root:/bin/bash")?;
            writeln!(fp, "{username}:x:{uid}:{gid}:{gecos}:{home}:/bin/bash")?;
            writeln!(fp, "host:x:65534:65534:Host files:/:/")?;
            Ok(())
        })?;

        // tee() is maybe a bit more reasonable to use...
        let mut group = etc.tee("group")?;
        writeln!(group, "root:x:0:0:")?;
        writeln!(group, "{groupname}:x:{gid}:0:")?;
        writeln!(group, "host:x:65534:0:")?;
        drop(group);

        // write() also exists if you have a string...
        etc.write(
            "ld.so.conf",
            concat!(
                "include /run/flatpak/ld.so.conf.d/app-*.conf\n",
                "include /app/etc/ld.so.conf\n",
                "include /app/etc/ld.so.conf.d/*.conf\n",
                "/app/lib64\n",
                "/app/lib\n",
                "include /run/flatpak/ld.so.conf.d/runtime-*.conf\n",
                "/usr/lib64/pipewire-0.3/jack/\n",
            ),
        )?;

        Ok(())
    }

    fn populate_xdg_runtime_dir(&self, xdg_runtime_dir: DirBuilder, hostdir: &Path) -> Result<()> {
        let hostdir = open_dir(CWD, hostdir)?;

        if self.flags.contains("wayland") {
            xdg_runtime_dir.bind_file("wayland-0", &hostdir, "wayland-0")?;
        }

        if self.flags.contains("session-bus") {
            xdg_runtime_dir.bind_file("bus", &hostdir, "bus")?;
        }

        Ok(())
    }

    fn populate_run(&self, run: DirBuilder) -> Result<()> {
        if let Some(xdg_runtime_dir) = dirs::runtime_dir() {
            run.subdir("user", |user| {
                let uid = self.uid.as_raw().to_string();
                if self.flags.contains("xdg-runtime-dir") {
                    user.bind_dir(&uid, CWD, &xdg_runtime_dir)
                } else {
                    user.subdir(&uid, |dir| {
                        self.populate_xdg_runtime_dir(dir, &xdg_runtime_dir)
                    })
                }
            })?;
        }

        run.bind_dir("host", CWD, "/")
    }

    fn populate_root(&self, root: DirBuilder) -> Result<()> {
        root.symlink("bin", "usr/bin")?;
        root.symlink("lib", "usr/lib")?;
        root.symlink("lib64", "usr/lib64")?;
        root.symlink("sbin", "usr/sbin")?;

        root.subdir("dev", |dev| self.populate_dev(dev))?;
        root.subdir("etc", |etc| self.populate_etc(etc))?;
        root.subdir("run", |run| self.populate_run(run))?;
        root.subdir("var", |var| var.symlink("run", "../run"))?;
        root.bind_dir("proc", CWD, "/proc")?;
        root.bind_dir("sys", CWD, "/sys")?;
        root.mkdir("tmp", 0o1777)?;

        root.mount("usr", mount_fuse_composefs(&self.runtime, &self.repo)?)?;
        if let Some(app) = &self.app {
            root.mount("app", mount_fuse_composefs(app, &self.repo)?)?;
        }

        if let Some(rel) = self.home.strip_prefix("/") {
            if self.flags.contains("home") {
                root.bind_dir(rel, CWD, &self.home)?;
            } else {
                fchown(
                    root.create_dir(rel, 0o755, false)?,
                    Some(self.uid),
                    Some(self.gid),
                )?;
            }
        }

        Ok(())
    }

    fn create_rootfs(&self) -> Result<MountHandle> {
        let root = mount_tmpfs("flatpak-root")
            .context("Failed to mount tmpfs for sandbox root filesystem")?;

        // Take this out later.  Only needed for kernels < 6.15.
        root.move_to(CWD, "/tmp")?;

        self.populate_root(DirBuilder::new(dup(&root.mountfd)?))?;

        Ok(root)
    }
}

/// Run the app after the sandbox has been established.
fn run_app(
    app: Option<&str>,
    runtime: &str,
    repo: &Arc<Repository<impl FsVerityHashValue>>,
    command: &str,
    args: &[&str],
) -> Result<Never> {
    let sandbox = SandboxConfig {
        repo: Arc::clone(repo),
        app: app.map(str::to_string),
        runtime: runtime.to_string(),
        username: whoami::username(),
        groupname: whoami::username(), // *shrug*
        gecos: whoami::realname(),
        uid: getuid(),
        gid: getgid(),
        home: dirs::home_dir().unwrap().to_str().unwrap().to_string(),
        flags: HashSet::from(
            ["home", "xdg-runtime-dir", "wayland", "session-bus"].map(|s| s.to_string()),
        ),
    };

    sandbox.unshare()?;
    sandbox.create_rootfs()?.pivot_root()?;

    Command::new("ldconfig")
        .arg("-X")
        .status()
        .context("Unable to run ldconfig")?;

    sandbox.drop_capabilities()?;

    let status = Command::new(command)
        .args(args)
        .current_dir(sandbox.home)
        .env("XDG_CONFIG_DIRS", "/app/etc/xdg:/etc/xdg")
        .env("GI_TYPELIB_PATH", "/app/lib64/girepository-1.0")
        .env(
            "GST_PLUGIN_SYSTEM_PATH",
            "/app/lib64/gstreamer-1.0:/usr/lib64/extensions/gstreamer-1.0:/usr/lib64/gstreamer-1.0",
        )
        .env(
            "XDG_DATA_DIRS",
            "/app/share:/usr/share:/usr/share/runtime/share:/run/host/user-share:/run/host/share",
        )
        .env("PATH", "/app/bin:/usr/bin")
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
