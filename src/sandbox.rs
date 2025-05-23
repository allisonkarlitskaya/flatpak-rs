use core::ops::Range;
use std::{
    collections::HashSet,
    ffi::OsStr,
    fmt,
    fs::File,
    io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Write as IoWrite},
    path::Path,
    process::{Command, exit},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use composefs::{fsverity::FsVerityHashValue, repository::Repository, tree::RegularFile};
use rustix::{
    fd::{AsFd, AsRawFd, OwnedFd},
    fs::{CWD, Gid, Mode, OFlags, Uid, fchown, mkdirat, open, openat, symlinkat},
    io::{Errno, write},
    mount::{
        FsMountFlags, FsOpenFlags, MountAttrFlags, MountPropagationFlags, MoveMountFlags,
        OpenTreeFlags, UnmountFlags, fsconfig_create, fsconfig_set_fd, fsconfig_set_flag,
        fsconfig_set_string, fsmount, fsopen, move_mount, open_tree, unmount,
    },
    path::Arg as PathArg,
    process::{fchdir, getgid, getpid, getuid, pivot_root},
    termios::ttyname,
    thread::{UnshareFlags, set_thread_gid, set_thread_groups, set_thread_uid, unshare},
};

use composefs_fuse::{open_fuse, serve_tree_fuse};

use crate::{manifest::Manifest, mount_setattr::mount_setattr, r#ref::Ref};

// ! is still experimental, so let's use this instead.
enum Never {}

#[derive(Debug)]
enum MappingType {
    #[allow(dead_code)]
    NoPreserve,     // flat map of the subrange
    #[allow(dead_code)]
    PreserveAsRoot, // preserve the "outside" uid/gid as 0:0
    PreserveAsUser, // preserve the "outside" uid/gid as the target user
}

#[derive(Debug)]
enum SandboxType {
    #[allow(dead_code)]
    Simple,                      // single uid/gid mapping
    #[allow(dead_code)]
    RequireMapping(MappingType), // require newuidmap/newgidmap
    TryMapping(MappingType),     // use newuidmap/newgidmap if available
}

#[derive(Debug, Eq, Hash, PartialEq)]
enum ShareFlags {
    Home,
    XdgRuntimeDir,
    SessionBus,
    Wayland,
}

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

fn mount_tmpfs(name: &str, mode: u16) -> Result<MountHandle> {
    FsHandle::open("tmpfs")?
        .set_string("source", name)?
        .set_mode("mode", mode)?
        .mount()
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
    r#ref: &Ref,
    repo: &Arc<Repository<impl FsVerityHashValue>>,
) -> Result<(Manifest, MountHandle)> {
    let dev_fuse = open_fuse()?;

    // Create the mount
    let mount = FsHandle::open("fuse")?
        .set_flag("ro")?
        //.set_flag("default_permissions")?
        .set_flag("allow_other")?
        .set_string("source", &format!("composefs-fuse:{ref}"))?
        .set_fd_str("fd", &dev_fuse)?
        .set_mode("rootmode", 0o40555)?
        .set_int("user_id", getuid().as_raw())?
        .set_int("group_id", getgid().as_raw())?
        .mount()?;

    // Spawn the server thread.  Awkwardly, we need to do the actual building of the image inside
    // of the thread because Filesystem isn't Send or Sync, owing to its use of Rc.  We use a mpsc
    // to pass the result back, along with the manifest (which we also want to extract).
    let repo = Arc::clone(repo);
    let name = format!("refs/flatpak-rs/{ref}");

    let (tx, rx) = std::sync::mpsc::channel::<Result<Manifest>>();

    std::thread::spawn(move || {
        let read_fs_and_metadata = || {
            let filesystem = composefs_oci::image::create_filesystem(&repo, &name, None)?;
            let manifest = match filesystem.root.get_file("metadata".as_ref())? {
                RegularFile::Inline(data) => data.clone().into_vec(),
                RegularFile::External(id, ..) => {
                    let mut data = vec![];
                    File::from(repo.open_object(id)?).read_to_end(&mut data)?;
                    data
                }
            };

            let manifest = Manifest::new(
                std::str::from_utf8(&manifest).context("Flatpak manifest is not valid utf-8")?,
            )?;

            Ok((filesystem, manifest))
        };

        let filesystem = match read_fs_and_metadata() {
            Ok((filesystem, manifest)) => {
                tx.send(Ok(manifest)).unwrap();
                filesystem
            }
            Err(err) => {
                tx.send(Err(err)).unwrap();
                return;
            }
        };

        let files = filesystem
            .root
            .get_directory("files".as_ref())
            .expect("no files");

        if let Err(err) = serve_tree_fuse(dev_fuse, files, &repo) {
            log::error!("FUSE server for composefs:{name} terminated irregularly: {err}");
        }
    });

    let manifest = rx.recv()??;

    Ok((manifest, mount))
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

struct DirBuilder<'a> {
    dirfd: &'a OwnedFd,
}

fn filter_errno<T>(result: rustix::io::Result<T>, ignored: Errno) -> rustix::io::Result<Option<T>> {
    match result {
        Ok(result) => Ok(Some(result)),
        Err(err) if err == ignored => Ok(None),
        Err(err) => Err(err),
    }
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

    fn new(dirfd: &'a OwnedFd) -> Self {
        Self { dirfd }
    }

    fn create_dir(&self, name: &str, mode: u32, exist_ok: bool) -> Result<OwnedFd> {
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

    fn create_file(&self, name: &str) -> Result<OwnedFd> {
        let (dirfd, name) = if let Some((parent, name)) = name.rsplit_once('/') {
            (&self.create_dir(parent, Self::DIR_PERMISSION, true)?, name)
        } else {
            (self.dirfd, name)
        };

        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        openat(dirfd, name, flags, Self::FILE_PERMISSION.into())
            .with_context(|| format!("Failed to open {name:?} for writing"))
    }

    fn subdir(&self, name: &str, populate: impl Fn(DirBuilder) -> Result<()>) -> Result<()> {
        populate(DirBuilder {
            dirfd: &self.create_dir(name, Self::DIR_PERMISSION, false)?,
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
        symlinkat(target, self.dirfd, name)
            .with_context(|| format!("Failed to symlink {name:?} -> {target:?}"))
    }

    fn mount(&self, name: &str, mnt: MountHandle) -> Result<()> {
        mnt.move_to(self.create_dir(name, Self::DIR_PERMISSION, false)?, "")
    }

    fn bind_dir(&self, name: &str, from_dirfd: impl AsFd, from_name: impl PathArg) -> Result<()> {
        self.mount(name, MountHandle::clone_recursive(from_dirfd, from_name)?)
    }

    fn bind_file(&self, name: &str, from_dirfd: impl AsFd, from_name: impl PathArg) -> Result<()> {
        MountHandle::clone(from_dirfd, from_name)?.move_to(self.create_file(name)?, "")
    }
}

fn find_range(filename: &str, username: &str) -> Result<Option<Range<u32>>> {
    let file = match File::open(filename) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => Err(err).context(format!("Failed to open {filename}"))?,
    };

    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("Failed to read from {filename}"))?;
        let mut parts = line.split(':');
        if parts.next() == Some(username) {
            let mut u32_parts = parts.map(str::parse::<u32>);
            match (u32_parts.next(), u32_parts.next()) {
                (Some(Ok(start)), Some(Ok(len))) => return Ok(Some(start..(start + len))),
                _ => bail!("Incorrectly formatted line in {filename}: {line}"),
            }
        }
    }

    Ok(None)
}

fn compute_mapping(mut subrange: Range<u32>, preserve: Option<(u32, u32)>) -> Vec<u32> {
    let mut result = vec![];
    let mut covered = 0;

    if let Some((preserve_inside, preserve_outside)) = preserve {
        let before_len = std::cmp::min(subrange.end - subrange.start, preserve_inside);
        if before_len > 0 {
            result.extend_from_slice(&[covered, subrange.start, before_len]);
            subrange = subrange.start + before_len..subrange.end;
            covered += before_len;
        }

        result.extend_from_slice(&[preserve_inside, preserve_outside, 1]);
        covered += 1;
    }

    if !subrange.is_empty() {
        result.extend_from_slice(&[covered, subrange.start, subrange.end - subrange.start]);
    }

    result
}

fn flatten<T: ToString>(values: &[T]) -> String {
    values
        .iter()
        .map(T::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

fn unshare_userns_newuidmap_newgidmap(uid: u32, gid: u32, mapping: &MappingType) -> Result<bool> {
    let username = whoami::username();
    let uid_range = find_range("/etc/subuid", &username)?;
    let gid_range = find_range("/etc/subgid", &username)?;
    let pid = rustix::process::Pid::as_raw(Some(getpid()));

    let (Some(uid_range), Some(gid_range)) = (uid_range, gid_range) else {
        // We can't do it this way, so abort before we start trying.
        return Ok(false);
    };

    let (uid_preserve, gid_preserve) = match mapping {
        MappingType::NoPreserve => (None, None),
        MappingType::PreserveAsRoot => (Some((0, getuid().as_raw())), Some((0, getgid().as_raw()))),
        MappingType::PreserveAsUser => (
            Some((uid, getuid().as_raw())),
            Some((gid, getgid().as_raw())),
        ),
    };

    // We're committed now.  We either succeed or fail.  Compute our mappings.
    let uidmap = flatten(&compute_mapping(uid_range, uid_preserve));
    let gidmap = flatten(&compute_mapping(gid_range, gid_preserve));

    // We can avoid fork() by using a small shell helper.  It remains in the original user
    // namespace, waits until we write a line to its stdin and then does the uid mapping for us.
    // We write that line after we unshare our namespace, and then wait the process to make sure
    // everything went OK.
    let mut cmd = Command::new("sh")
        .stdin(std::process::Stdio::piped())
        .arg("-cxe")
        .arg(format!(
            "read; newuidmap {pid} {uidmap}; newgidmap {pid} {gidmap};"
        ))
        .spawn()?;

    unshare(UnshareFlags::NEWUSER).context("Unable to create new user namespace")?;

    // Write a line to stdin to cause the 'read' in the above shell script to finish.
    // SAFETY: We know we did .stdin() with a pipe, above, so this will not panic.
    writeln!(cmd.stdin.take().unwrap())?;

    match cmd.wait().context("Unable to run newuidmap")?.code() {
        Some(0) => {}
        _other => {
            panic!("uidmap failed");
        }
    };

    // The POSIX security model says that we shouldn't be allowed to drop groups, but newgidmap
    // blows a giant hole in that by installing a gid_map without first setting setgroup to "deny".
    // I guess we can drop our extra groups, after all...
    set_thread_groups(&[]).context("Unable to setgroups([])")?;

    // With a mapped UID range present we can do our setup procedure as uid/gid 0:0
    set_thread_uid(Uid::ROOT).context("Unable to setuid(0)")?;
    set_thread_gid(Gid::ROOT).context("Unable to setgid(0)")?;

    Ok(true)
}

fn unshare_userns_simple(inside_uid: u32, inside_gid: u32) -> Result<()> {
    let uid = getuid().as_raw();
    let gid = getgid().as_raw();

    // See user_namespaces(7): we must not be permitted to drop groups (as viewed from the
    // parent namespace) or else we might be able to gain permissions (imagine a file where the
    // "group" access rights are less than "other").  As such, since we're setting our own
    // "gid_map", we need to write "deny" to "setgroups" before we can write a "gid_map" (which
    // would otherwise enable the setgroups() call).  If we were using newgidmap we could
    // circumvent this, but we're not.
    //
    // This basically means that if the calling user was carrying extra groups, they'll have
    // these groups show up as "nobody" inside the sandbox.

    unshare(UnshareFlags::NEWUSER).context("Unable to create new user namespace")?;
    write_to("/proc/self/uid_map", &format!("{inside_uid} {uid} 1\n"))?;
    write_to("/proc/self/setgroups", "deny\n")?;
    write_to("/proc/self/gid_map", &format!("{inside_gid} {gid} 1\n"))?;

    // We started out as {uid} and mapped that to {inside_uid} (ditto for gid) so we're now running
    // as inside_uid:inside_gid but with all capabilities present.  We'll do our setup like this.
    // Later: we remount / as read-only and call setuid()/setgid() to drop capabilities.

    Ok(())
}

struct Sandbox {
    sandbox_type: SandboxType,
    uid: Uid,
    gid: Gid,

    username: String,
    groupname: String,
    gecos: String,
    home: String,

    share: HashSet<ShareFlags>,
}

impl Sandbox {
    fn unshare(&self) -> Result<()> {
        let inside_uid = self.uid.as_raw();
        let outside_gid = self.gid.as_raw();

        // Unshare user namespace
        match &self.sandbox_type {
            SandboxType::Simple => unshare_userns_simple(inside_uid, outside_gid)?,
            SandboxType::RequireMapping(mapping_type) => {
                if !unshare_userns_newuidmap_newgidmap(inside_uid, outside_gid, mapping_type)? {
                    bail!("Unable to find usable subuid/subgid ranges and mapping is required");
                }
            }
            SandboxType::TryMapping(mapping_type) => {
                if !unshare_userns_newuidmap_newgidmap(inside_uid, outside_gid, mapping_type)? {
                    unshare_userns_simple(inside_uid, outside_gid)?;
                }
            }
        }

        // Unshare mount namespace
        unshare(UnshareFlags::NEWNS).context("Unable to create new mount namespace")?;

        // Unshare PID namespace: we can't do that because of our FUSE threads
        // unshare(UnshareFlags::NEWPID).context("Unable to create new pid namespace")?;

        Ok(())
    }

    fn drop_capabilities(&self) -> Result<()> {
        set_thread_gid(self.gid).with_context(|| format!("Unable to setgid({:?})", self.gid))?;
        set_thread_uid(self.uid).with_context(|| format!("Unable to setuid({:?})", self.uid))?;
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
        dev.mount("shm", mount_tmpfs("shm", 0o1777)?)?;

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

        if self.share.contains(&ShareFlags::Wayland) {
            xdg_runtime_dir.bind_file("wayland-0", &hostdir, "wayland-0")?;
        }

        if self.share.contains(&ShareFlags::SessionBus) {
            xdg_runtime_dir.bind_file("bus", &hostdir, "bus")?;
        }

        Ok(())
    }

    fn populate_run(&self, run: DirBuilder) -> Result<()> {
        if let Some(xdg_runtime_dir) = dirs::runtime_dir() {
            run.subdir("user", |user| {
                let uid = self.uid.as_raw().to_string();
                if self.share.contains(&ShareFlags::XdgRuntimeDir) {
                    user.bind_dir(&uid, CWD, &xdg_runtime_dir)
                } else {
                    user.subdir(&uid, |dir| {
                        self.populate_xdg_runtime_dir(dir, &xdg_runtime_dir)
                    })
                }
            })?;
        }

        //run.bind_dir("host", CWD, "/");

        Ok(())
    }

    fn populate_root(&self, root: &DirBuilder) -> Result<()> {
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
        root.mount("tmp", mount_tmpfs("tmp", 0o1777)?)?;

        if let Some(rel) = self.home.strip_prefix("/") {
            if self.share.contains(&ShareFlags::Home) {
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

    fn create_rootfs(
        &self,
        app_mount: Option<MountHandle>,
        usr_mount: MountHandle,
    ) -> Result<MountHandle> {
        let rootmnt = mount_tmpfs("flatpak-root", 0o755)
            .context("Failed to mount tmpfs for sandbox root filesystem")?;

        // TODO: Take this out later.  Only needed for kernels < 6.15.
        rootmnt.move_to(CWD, "/tmp")?;

        let root = DirBuilder::new(&rootmnt.mountfd);
        self.populate_root(&root)?;

        root.mount("usr", usr_mount)?;
        if let Some(app) = app_mount {
            root.mount("app", app)?;
        }

        Ok(rootmnt)
    }

    fn run(
        &self,
        repo: &Arc<Repository<impl FsVerityHashValue>>,
        r#ref: &Ref,
        command: Option<&str>,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> Result<Never> {
        // Unshare namespaces
        self.unshare()?;

        // We need to mount the fuse filesystems after the unshare() because they run in threads and we
        // can't unshare the userns in a process with threads.
        let (app_manifest, app_mount, runtime_manifest, usr_mount) = if r#ref.is_app() {
            let (app_manifest, app_mount) = mount_fuse_composefs(r#ref, repo)?;
            let (runtime_manifest, usr_mount) =
                mount_fuse_composefs(&app_manifest.get_runtime()?, repo)?;
            (
                Some(app_manifest),
                Some(app_mount),
                runtime_manifest,
                usr_mount,
            )
        } else {
            let (runtime_manifest, usr_mnt) = mount_fuse_composefs(r#ref, repo)?;
            (None, None, runtime_manifest, usr_mnt)
        };

        // Build our rootfs and pivot into it
        let rootfs = self.create_rootfs(app_mount, usr_mount)?;
        rootfs.pivot_root()?;

        // TODO: apparently we should cache this...
        Command::new("ldconfig")
            .arg("-X")
            .status()
            .context("Unable to run ldconfig")?;

        // No more changes: make the rootfs readonly and change to the target uid/gid
        rootfs.make_readonly()?;
        self.drop_capabilities()?;

        let command = if let Some(command) = command {
            command
        } else if let Some(manifest) = app_manifest.as_ref() {
            manifest.get("Application", "command")?
        } else {
            "/bin/sh"
        };

        // Run our command
        let status = Command::new(command)
            .args(args)
            .envs(runtime_manifest.get_environment()?)
            .env("PATH", "/app/bin:/usr/bin")
            .env("FLATPAK_ID", r#ref.get_id())
            .env("PS1", "[📦 $FLATPAK_ID \\W]\\$ ")
            .current_dir(&self.home)
            .status()
            .context("Unable to spawn /bin/sh")?;

        if let Some(code) = status.code() {
            exit(code);
        } else {
            exit(255);
        }
    }
}

pub(crate) fn run_sandboxed(
    repo: &Arc<Repository<impl FsVerityHashValue>>,
    r#ref: &Ref,
    command: Option<&str>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> ! {
    let sandbox = Sandbox {
        sandbox_type: SandboxType::TryMapping(MappingType::PreserveAsUser),
        username: whoami::username(),
        groupname: whoami::username(), // *shrug*
        gecos: whoami::realname(),
        uid: getuid(),
        gid: getgid(),
        home: dirs::home_dir().unwrap().to_str().unwrap().to_string(),
        share: HashSet::from([ShareFlags::Home, ShareFlags::XdgRuntimeDir]),
    };

    match sandbox.run(repo, r#ref, command, args) {
        Err(err) => panic!("Failed to execute app in sandbox: {err}"),
    }
}
