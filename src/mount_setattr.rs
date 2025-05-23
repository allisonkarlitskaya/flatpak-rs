// https://github.com/bytecodealliance/rustix/pull/1002

use rustix::{
    fd::{AsFd, AsRawFd},
    ffi::{c_char, c_int, c_uint},
    fs::AtFlags,
    mount::{MountAttrFlags, MountPropagationFlags},
};

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

pub(crate) fn mount_setattr(
    dirfd: impl AsFd,
    attr_set: MountAttrFlags,
    attr_clr: MountAttrFlags,
    propagation: MountPropagationFlags,
) -> std::io::Result<()> {
    let attr = MountAttr {
        attr_set: attr_set.bits() as u64,
        attr_clr: attr_clr.bits() as u64,
        propagation: propagation.bits() as u64,
        userns_fd: 0,
    };

    match unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            dirfd.as_fd().as_raw_fd() as c_int,
            b"\0".as_ptr() as *const c_char,
            AtFlags::EMPTY_PATH.bits() as c_uint,
            &attr as *const MountAttr,
            std::mem::size_of_val(&attr) as usize,
        )
    } {
        0 => Ok(()),
        -1 => Err(std::io::Error::last_os_error()),
        _ => unreachable!(),
    }
}
