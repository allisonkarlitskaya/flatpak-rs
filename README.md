# flatpak-rs

This is a toy to validate the API of the composefs-rs project for various
users.  It's currently capable of listing, searching and pulling apps from OCI
flatpak repositories.  It also has initial sandbox support (internal, not
bwrap-based) and can "enter" runtimes and apps.

There's almost no support for forwarding things like Wayland sockets and such,
so applications don't actually work.

You can use it like this:

```
$ cargo run --release search maps
...
app/org.gnome.Maps/x86_64/stable

$ cargo run --release install app/org.gnome.Maps/x86_64/stable
...
Now: enter 5c2d5daf4a924b2dc85072e2a92e68da7b17138e0f2793802d9ea2bf3f0e95b6 5b92ac45f0ebd6bb29e0cfe33811b0f3de50a17fd03737724e35f8bceb34f36f

$ cargo run --release enter 5c2d5daf4a924b2dc85072e2a92e68da7b17138e0f2793802d9ea2bf3f0e95b6 5b92ac45f0ebd6bb29e0cfe33811b0f3de50a17fd03737724e35f8bceb34f36f
[📦 org.flatpak.test /]# findmnt -o TARGET,SOURCE,FSTYPE
TARGET                         SOURCE                                   FSTYPE
/                              none                                     tmpfs
├─/dev/null                    devtmpfs[/null]                          devtmpfs
├─/dev/zero                    devtmpfs[/zero]                          devtmpfs
├─/dev/full                    devtmpfs[/full]                          devtmpfs
├─/dev/random                  devtmpfs[/random]                        devtmpfs
├─/dev/urandom                 devtmpfs[/urandom]                       devtmpfs
├─/dev/tty                     devtmpfs[/tty]                           devtmpfs
├─/dev/shm                     none                                     tmpfs
├─/proc                        proc                                     proc
│ └─/proc/sys/fs/binfmt_misc   systemd-1                                autofs
│   └─/proc/sys/fs/binfmt_misc binfmt_misc                              binfmt_misc
├─/sys                         sysfs                                    sysfs
│ ├─/sys/kernel/security       securityfs                               securityfs
│ ├─/sys/fs/cgroup             cgroup2                                  cgroup2
│ │ └─/sys/fs/cgroup           cgroup2                                  cgroup2
│ ├─/sys/fs/pstore             pstore                                   pstore
│ ├─/sys/firmware/efi/efivars  efivarfs                                 efivarfs
│ ├─/sys/fs/bpf                bpf                                      bpf
│ ├─/sys/kernel/config         configfs                                 configfs
│ ├─/sys/fs/selinux            selinuxfs                                selinuxfs
│ │ └─/sys/fs/selinux          fuse-overlayfs[/usr/share/empty]         fuse.fuse-overlayfs
│ ├─/sys/kernel/debug          debugfs                                  debugfs
│ ├─/sys/kernel/tracing        tracefs                                  tracefs
│ └─/sys/fs/fuse/connections   fusectl                                  fusectl
├─/usr                         composefs-fuse                           fuse
├─/etc                         fuse-overlayfs[/usr/etc]                 fuse.fuse-overlayfs
│ └─/etc/resolv.conf           tmpfs[/systemd/resolve/stub-resolv.conf] tmpfs
└─/app                         composefs-fuse                           fuse
[📦 org.flatpak.test /]# ls -l /app/bin
total 1
lrwxrwxrwx 1 0 0 0 Apr 16 04:41 gnome-maps -> /app/share/gnome-maps/org.gnome.Maps
[📦 org.flatpak.test /]# cat /usr/lib/os-release
NAME="Fedora Linux"
VERSION="42 (Flatpak runtime)"
...
```

This currently depends on an unmerged composefs-rs branch.  See
https://github.com/containers/composefs-rs/pull/130 for more info.
