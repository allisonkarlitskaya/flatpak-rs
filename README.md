# flatpak-rs

This is a toy to validate the API of the composefs-rs project for various
users.  It's currently capable of listing, searching and pulling apps from OCI
flatpak repositories.  It also has initial sandbox support (internal, not
bwrap-based) and can "enter" runtimes and apps, and even run simple apps.

You can use it like this:

```
$ cargo run --release search mahjongg
...
app/org.kde.kmahjongg/x86_64/stable
app/org.gnome.Mahjongg/x86_64/stable

$ cargo run --release install app/org.gnome.Mahjongg/x86_64/stable
...

$ cargo run --release run --command=/bin/sh app/org.gnome.Mahjongg/x86_64/stable
...
`
[📦 org.gnome.Mahjongg ~]$ findmnt -o TARGET,SOURCE
TARGET                                   SOURCE
/                                        flatpak-root
├─/app                                   composefs-fuse:app/org.gnome.Mahjongg/x86_64/stable
├─/usr                                   composefs-fuse:runtime/org.fedoraproject.Platform/x86_64/f42
├─/dev/full                              devtmpfs[/full]
├─/dev/null                              devtmpfs[/null]
├─/dev/random                            devtmpfs[/random]
├─/dev/tty                               devtmpfs[/tty]
├─/dev/urandom                           devtmpfs[/urandom]
├─/dev/zero                              devtmpfs[/zero]
├─/dev/console                           devpts[/16]
├─/dev/pts                               none
├─/dev/shm                               shm
├─/etc/resolv.conf                       tmpfs[/systemd/resolve/stub-resolv.conf]
├─/etc/localtime                         composefs[/usr/share/zoneinfo/Europe/Berlin]
├─/run/user/1000                         tmpfs
│ ├─/run/user/1000/gvfs                  gvfsd-fuse
│ └─/run/user/1000/doc                   portal
├─/proc                                  proc
│ └─/proc/sys/fs/binfmt_misc             systemd-1
│   └─/proc/sys/fs/binfmt_misc           binfmt_misc
├─/sys                                   sysfs
│ ├─/sys/kernel/security                 securityfs
│ ├─/sys/fs/cgroup                       cgroup2
│ │ └─/sys/fs/cgroup                     cgroup2
│ ├─/sys/fs/pstore                       pstore
│ ├─/sys/firmware/efi/efivars            efivarfs
│ ├─/sys/fs/bpf                          bpf
│ ├─/sys/kernel/config                   configfs
│ ├─/sys/fs/selinux                      selinuxfs
│ │ └─/sys/fs/selinux                    fuse-overlayfs[/usr/share/empty]
│ ├─/sys/kernel/debug                    debugfs
│ ├─/sys/kernel/tracing                  tracefs
│ └─/sys/fs/fuse/connections             fusectl
├─/tmp                                   tmp
└─/var/home/lis                          /dev/nvme0n1p3[/home/lis]
[📦 org.gnome.Mahjongg ~]$ ls -l /app/bin
total 1
-rwxr-xr-x 1 host host 2549800 Apr 16 06:43 gnome-mahjongg
[📦 org.gnome.Mahjongg ~]$ cat /usr/lib/os-release 
NAME="Fedora Linux"
VERSION="42 (Flatpak runtime)"
...
[📦 org.gnome.Mahjongg ~]$ exit
exit
$ cargo run --release run app/org.gnome.Mahjongg/x86_64/stable  # ...and play a game
```
