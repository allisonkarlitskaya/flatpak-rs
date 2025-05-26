use std::os::unix::process::CommandExt;

use rustix::{
    fd::{BorrowedFd, OwnedFd},
    fs::readlink,
    io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd},
};

pub(super) trait WithFds {
    fn with_fds(&mut self, map: impl Into<Box<[OwnedFd]>>) -> &mut Self;
}

impl WithFds for std::process::Command {
    fn with_fds(&mut self, fds: impl Into<Box<[OwnedFd]>>) -> &mut Self {
        let fds = fds.into();
        unsafe {
            self.pre_exec(move || {
                // Perform paranoid checking to try to catch non-O_CLOEXEC fds
                for fd in 3..1000 {
                    match fcntl_getfd(BorrowedFd::borrow_raw(fd)) {
                        Err(Errno::BADF) => {
                            /* Expected: this failed because this fd is not open */
                        }
                        Ok(flags) if flags.contains(FdFlags::CLOEXEC) => {
                            /* Expected: this fd is correctly marked CLOEXEC */
                        }
                        Err(err) => {
                            /* Unexpected error */
                            panic!("Unable to read flags for fd {fd}: {err:?}");
                        }
                        Ok(flags) => {
                            /* CLOEXEC is missing */
                            let target = readlink(format!("/proc/self/fd/{fd}"), [])?;
                            panic!("Missing O_CLOEXEC on fd {fd} -> {target:?}: {flags:?}");
                        }
                    }
                }

                // Mark all of our inheritable fds as non-CLOEXEC
                for fd in fds.iter() {
                    let flags = fcntl_getfd(fd)?;
                    fcntl_setfd(fd, flags - FdFlags::CLOEXEC)?;
                }

                Ok(())
            });
            self
        }
    }
}
