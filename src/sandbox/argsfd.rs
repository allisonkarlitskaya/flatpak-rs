use anyhow::{Context, Result, ensure};
use rustix::{
    fd::{AsRawFd, OwnedFd},
    io::{IoSlice, writev},
    pipe::{PipeFlags, pipe_with},
};

// Just store things directly in the pipe.
pub(super) struct ArgsFdBuilder {
    read: OwnedFd,
    write: OwnedFd,
}

impl ArgsFdBuilder {
    pub(super) fn new() -> Result<Self> {
        // We store directly into the pipe as we get the arguments under the assumption that we'll
        // have more than enough space: the default size is 64KiB.  If it fills up, we want to get
        // an error about it, so let's use NONBLOCK: we need to handle errors in the .add() case
        // anyway because of checking for "\0".
        let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)
            .context("Unable to create a pipe")?;
        Ok(Self { read, write })
    }

    pub(super) fn add(&self, arg: impl AsRef<[u8]>) -> Result<()> {
        let arg = arg.as_ref();
        ensure!(
            arg.iter().all(|c| *c != 0),
            "Cannot add commandline argument to argfd containing nuls"
        );
        let iovec = [IoSlice::new(arg), IoSlice::new(b"\0")];
        writev(&self.write, &iovec)?;
        Ok(())
    }

    pub(super) fn extend(&self, args: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Result<()> {
        for arg in args {
            self.add(arg)?
        }
        Ok(())
    }

    pub(super) fn done(self) -> OwnedFd {
        // We drop the writer so the reader can successfully read to EOF
        self.read
    }
}

pub(super) trait ArgsFd {
    fn as_arg(&self) -> String;
}

impl ArgsFd for OwnedFd {
    fn as_arg(&self) -> String {
        format!("--args={}", self.as_raw_fd())
    }
}
