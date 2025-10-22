//! This basically implements Process from:
//! <https://github.com/rust-lang/rust/blob/master/library/std/src/sys/unix/process/process_unix.rs>

use libc::c_int;
use std::fmt::{Debug, Formatter, Result as FmtResult};
use std::io::{Error, Result};

use libc::pid_t;

use crate::cvt::{cvt, cvt_r};

pub struct Process {
    pid: pid_t,
    status: Option<ExitStatus>,
}

impl Process {
    pub unsafe fn new(pid: pid_t) -> Self {
        // Safety: If `pidfd` is nonnegative, we assume it's valid and otherwise unowned.
        Process { pid, status: None }
    }

    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    pub fn kill(&mut self) -> Result<()> {
        // If we've already waited on this process then the pid can be recycled
        // and used for another process, and we probably shouldn't be killing
        // random processes, so just return an error.
        if self.status.is_some() {
            Err(Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid argument: can't kill an exited process",
            ))
        } else {
            cvt(unsafe { libc::kill(self.pid, libc::SIGKILL) }).map(drop)
        }
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let mut status = 0 as c_int;
        cvt_r(|| unsafe { libc::waitpid(self.pid, &mut status, 0) })?;
        self.status = Some(ExitStatus::new(status));
        Ok(ExitStatus::new(status))
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut status = 0 as c_int;
        let pid = cvt(unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) })?;
        if pid == 0 {
            Ok(None)
        } else {
            self.status = Some(ExitStatus::new(status));
            Ok(Some(ExitStatus::new(status)))
        }
    }
}

/// Describes the result of a process after it has terminated.
#[derive(PartialEq, Eq, Clone, Copy)]
pub struct ExitStatus(c_int);

impl Debug for ExitStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_tuple("unix_wait_status").field(&self.0).finish()
    }
}

impl ExitStatus {
    pub(crate) fn new(status: c_int) -> ExitStatus {
        ExitStatus(status)
    }

    fn exited(&self) -> bool {
        libc::WIFEXITED(self.0)
    }

    /// Was termination successful? Returns a Result.
    pub fn exit_ok(&self) -> Result<()> {
        // This assumes that WIFEXITED(status) && WEXITSTATUS==0 corresponds to status==0.  This is
        // true on all actual versions of Unix, is widely assumed, and is specified in SuS
        // https://pubs.opengroup.org/onlinepubs/9699919799/functions/wait.html .  If it is not
        // true for a platform pretending to be Unix, the tests (our doctests, and also
        // procsss_unix/tests.rs) will spot it.  `ExitStatusError::code` assumes this too.
        #[allow(clippy::useless_conversion)]
        match c_int::try_from(self.0) {
            /* was nonzero */
            Ok(failure) => Err(Error::other(
                format!("process exited with status {}", failure),
            )),
            /* was zero, couldn't convert */
            Err(_) => Ok(()),
        }
    }

    /// Was termination successful?
    ///
    /// Signal termination is not considered a success, and success is defined
    /// as a zero exit status.
    pub fn success(&self) -> bool {
        self.exit_ok().is_ok()
    }

    /// Returns the exit code of the process, if any.
    ///
    /// In Unix terms the return value is the exit status:
    /// the value passed to exit, if the process finished by calling exit.
    /// Note that on Unix the exit status is truncated to 8 bits, and that
    /// values that didn’t come from a program’s call to exit may be invented
    /// by the runtime system (often, for example, 255, 254, 127 or 126).
    ///
    /// This will return None if the process was terminated by a signal.
    /// ExitStatusExt is an extension trait for extracting any such signal,
    /// and other details, from the ExitStatus.
    pub fn code(&self) -> Option<i32> {
        self.exited().then(|| libc::WEXITSTATUS(self.0))
    }

    /// If the process was terminated by a signal, returns that signal.
    ///
    /// In other words, if WIFSIGNALED, this returns WTERMSIG.
    pub fn signal(&self) -> Option<i32> {
        libc::WIFSIGNALED(self.0).then(|| libc::WTERMSIG(self.0))
    }

    /// If the process was terminated by a signal, says whether it dumped core.
    pub fn core_dumped(&self) -> bool {
        libc::WIFSIGNALED(self.0) && libc::WCOREDUMP(self.0)
    }

    /// If the process was stopped by a signal, returns that signal.
    ///
    /// In other words, if WIFSTOPPED, this returns WSTOPSIG.
    /// This is only possible if the status came from a wait system call
    /// which was passed WUNTRACED, and was then converted into an ExitStatus.
    pub fn stopped_signal(&self) -> Option<i32> {
        libc::WIFSTOPPED(self.0).then(|| libc::WSTOPSIG(self.0))
    }

    /// Whether the process was continued from a stopped status.
    ///
    /// Ie, WIFCONTINUED. This is only possible if the status came from a
    /// wait system call which was passed WCONTINUED, and was then converted
    /// into an ExitStatus.
    pub fn continued(&self) -> bool {
        libc::WIFCONTINUED(self.0)
    }

    /// Returns the underlying raw wait status.
    ///
    /// The returned integer is a wait status, not an exit status.
    #[allow(clippy::wrong_self_convention)]
    pub fn into_raw(&self) -> c_int {
        self.0
    }
}

/// Converts a raw `c_int` to a type-safe `ExitStatus` by wrapping it without copying.
impl From<c_int> for ExitStatus {
    fn from(a: c_int) -> ExitStatus {
        ExitStatus(a)
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub struct ExitStatusError(c_int);

impl From<ExitStatusError> for ExitStatus {
    fn from(val: ExitStatusError) -> Self {
        ExitStatus(val.0)
    }
}

impl Debug for ExitStatusError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_tuple("unix_wait_status").field(&self.0).finish()
    }
}

impl ExitStatusError {}
