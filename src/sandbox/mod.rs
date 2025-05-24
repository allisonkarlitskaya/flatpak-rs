mod dirbuilder;
mod mount_setattr;
mod mounthandle;
mod util;
mod wayland;

use core::ops::Range;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs::File,
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    process::{Command, exit},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use composefs::{fsverity::FsVerityHashValue, repository::Repository, tree::RegularFile};
use composefs_fuse::{open_fuse, serve_tree_fuse};
use rustix::{
    fd::OwnedFd,
    fs::{CWD, Gid, Uid, fchown},
    io::Errno,
    process::{getgid, getpid, getuid},
    termios::ttyname,
    thread::{UnshareFlags, set_thread_gid, set_thread_groups, set_thread_uid, unshare},
};

use crate::{instance::Instance, manifest::Manifest, r#ref::Ref};

use self::{
    dirbuilder::DirBuilder,
    mounthandle::{FsHandle, MountHandle},
    util::{filter_errno, open_dir, write_to},
    wayland::bind_wayland_socket,
};

// ! is still experimental, so let's use this instead.
enum Never {}

#[derive(Debug)]
enum MappingType {
    #[allow(dead_code)]
    /// flat map of the subrange
    NoPreserve,
    #[allow(dead_code)]
    /// preserve the "outside" uid/gid as 0:0
    PreserveAsRoot,
    /// preserve the "outside" uid/gid as the target user
    PreserveAsUser,
}

#[derive(Debug)]
enum SandboxType {
    #[allow(dead_code)]
    /// single uid/gid mapping
    Simple,
    #[allow(dead_code)]
    /// require newuidmap/newgidmap
    RequireMapping(MappingType),
    /// use newuidmap/newgidmap if available
    TryMapping(MappingType),
}

#[derive(Debug, Eq, Hash, PartialEq)]
enum ShareFlags {
    Home,
    XdgRuntimeDir,
    SessionBus,
    Wayland,
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
    r#ref: Ref,
    instance: Instance,

    sandbox_type: SandboxType,
    uid: Uid,
    gid: Gid,

    username: String,
    groupname: String,
    gecos: String,
    home: String,

    share: HashSet<ShareFlags>,

    env: HashMap<&'static str, Option<String>>,
    fds: Vec<OwnedFd>,
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

    fn populate_runtime_dir(&mut self, runtime_dir: DirBuilder, hostdir: &OwnedFd) -> Result<()> {
        if self.share.contains(&ShareFlags::Wayland) {
            if let Some((name, close_fd)) = bind_wayland_socket(
                &runtime_dir,
                hostdir,
                self.r#ref.get_id(),
                self.instance.get_id(),
            )? {
                self.setenv("WAYLAND_DISPLAY", name);
                self.fds.extend(close_fd);
            }
        } else {
            self.unsetenv("WAYLAND_DISPLAY");
        }

        if self.share.contains(&ShareFlags::SessionBus) {
            runtime_dir.bind_file("bus", hostdir, "bus")?;
        }

        Ok(())
    }

    fn populate_run_user(&mut self, user: DirBuilder) -> Result<()> {
        let uid = self.uid.as_raw().to_string();
        let Some(xdg_runtime_dir) = dirs::runtime_dir() else {
            bail!("We require XDG_RUNTIME_DIR set on the host");
        };

        let hostdir = open_dir(CWD, &xdg_runtime_dir)
            .with_context(|| format!("Unable to open XDG_RUNTIME_DIR {xdg_runtime_dir:?}"))?;

        self.setenv("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));

        if self.share.contains(&ShareFlags::XdgRuntimeDir) {
            user.bind_dir(&uid, hostdir, "")
        } else {
            user.populate_mount(
                &uid,
                FsHandle::open("tmpfs")?
                    .set_string("source", "xdg-runtime-dir")?
                    .set_mode("mode", 0o700)?
                    .set_int("uid", self.uid.as_raw())?
                    .set_int("gid", self.gid.as_raw())?
                    .mount()?,
                |dir| self.populate_runtime_dir(dir, &hostdir),
            )
        }
    }

    fn populate_run(&mut self, run: DirBuilder) -> Result<()> {
        run.subdir("user", |user| self.populate_run_user(user))?;
        //run.bind_dir("host", CWD, "/");

        Ok(())
    }

    fn populate_root(&mut self, root: &DirBuilder) -> Result<()> {
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
        &mut self,
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

    fn setenv(&mut self, key: &'static str, value: impl Into<String>) {
        self.env.insert(key, Some(value.into()));
    }

    fn unsetenv(&mut self, key: &'static str) {
        self.env.insert(key, None);
    }

    fn run(
        &mut self,
        repo: &Arc<Repository<impl FsVerityHashValue>>,
        command: Option<&str>,
        args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    ) -> Result<Never> {
        // Unshare namespaces
        self.unshare()?;

        // We need to mount the fuse filesystems after the unshare() because they run in threads and we
        // can't unshare the userns in a process with threads.
        let (app_manifest, app_mount, runtime_manifest, usr_mount) = if self.r#ref.is_app() {
            let (app_manifest, app_mount) = mount_fuse_composefs(&self.r#ref, repo)?;
            let (runtime_manifest, usr_mount) =
                mount_fuse_composefs(&app_manifest.get_runtime()?, repo)?;
            (
                Some(app_manifest),
                Some(app_mount),
                runtime_manifest,
                usr_mount,
            )
        } else {
            let (runtime_manifest, usr_mnt) = mount_fuse_composefs(&self.r#ref, repo)?;
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
        let mut command = Command::new(command);
        command.args(args);
        command.current_dir(&self.home);
        command.envs(runtime_manifest.get_environment()?);

        for (key, value) in &self.env {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }

        command.env("PATH", "/app/bin:/usr/bin");
        command.env("FLATPAK_ID", self.r#ref.get_id());
        command.env("PS1", "[📦 $FLATPAK_ID \\W]\\$ ");

        let status = command
            .status()
            .with_context(|| format!("Unable to spawn {command:?}"))?;

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
    let mut sandbox = Sandbox {
        r#ref: r#ref.clone(),
        instance: Instance::new_pid(),

        sandbox_type: SandboxType::TryMapping(MappingType::PreserveAsUser),
        username: whoami::username(),
        groupname: whoami::username(), // *shrug*
        gecos: whoami::realname(),
        uid: getuid(),
        gid: getgid(),
        home: dirs::home_dir().unwrap().to_str().unwrap().to_string(),
        share: HashSet::from([ShareFlags::Home, ShareFlags::Wayland]),

        env: HashMap::new(),
        fds: Vec::new(),
    };

    match sandbox.run(repo, command, args) {
        Err(err) => panic!("Failed to execute app in sandbox: {err:?}"),
    }
}
