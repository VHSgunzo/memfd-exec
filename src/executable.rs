//! This essentially reimplements the code at:
//! which is an internal implementation of the code at:
//! <https://github.com/rust-lang/rust/blob/master/library/std/src/process.rs>
//! <https://github.com/rust-lang/rust/blob/master/library/std/src/sys/unix/process/process_unix.rs>
//! <https://github.com/rust-lang/rust/blob/master/library/std/src/sys/unix/process/process_common.rs>
//! for external use to provide a very similar interface to process::Command for in-memory executables

use std::{
    env,
    process::exit,
    time::Duration,
    mem::MaybeUninit,
    collections::BTreeMap,
    thread::{spawn, sleep},
    io::{Error, ErrorKind, Result},
    ffi::{CStr, CString, OsStr, OsString},
    path::{Path, PathBuf}, process, ptr::null_mut,
    fs::{self, create_dir_all, set_permissions, File, Permissions},
    os::{unix::{fs::PermissionsExt, prelude::{OsStrExt, OsStringExt}}, fd::{AsFd, AsRawFd}},
};

use libc::{close, pid_t, sigemptyset, signal};
use nix::{
    errno::Errno,
    fcntl::{open, OFlag},
    unistd::{access, fexecve, write, fork, setsid, AccessFlags, ForkResult},
    sys::{memfd::{memfd_create, MemFdCreateFlag}, stat::Mode, wait::waitpid},
};

use crate::{
    anon_pipe::anon_pipe,
    child::Child,
    command_env::CommandEnv,
    cvt::{cvt, cvt_nz, cvt_r},
    output::Output,
    process::{ExitStatus, Process},
    stdio::{ChildPipes, Stdio, StdioPipes},
};

/// This is the main struct used to create an in-memory only executable. Wherever possible, it
/// is intended to be a drop-in replacement for the standard library's `process::Command` struct.
///
/// # Examples
///
/// This example is the "motivating case" for this library. It shows how to execute a binary
/// entirely from memory, without writing it to disk. This is useful for executing binaries
/// sneakily, or (the real reason) for bundling executables that are a pain to set up and
/// compile, but whose static versions are very portable. Here's a "sneaky" example:
///
/// ```no_compile
/// use memfd_exec::{MemFdExecutable, Stdio};
///
/// // You can include the entirety of a binary (for example, nc)
/// let nc_binary= include_bytes!("/usr/bin/nc-static");
///
///
/// // The first argument is just the name for the program, you can pick anything but
/// // if the program expects a specific argv[0] value, use that.
/// // The second argument is the binary code to execute.
/// let mut cmd = MemFdExecutable::new("nc", nc_binary)
///     // We can pass arbitrary args just like with Command. Here, we'll execute nc
///     // to listen on a port and run a shell for connections, entirely from memory.
///     .arg("-l")
///     .arg("1234")
///     .arg("-e")
///     .arg("/bin/sh")
///     // And we can get piped stdin/stdout just like with Command
///     .stdout(Stdio::piped())
///     // Spawn starts the child process and gives us a handle back
///     .spawn()
///     .expect("failed to execute process");
///
/// // Then, we can wait for the program to exit.
/// cmd.wait();
/// ```
#[derive(Debug)]
pub struct MemFdExecutable<'a> {
    /// The contents of the ELF executable to run. This content can be included in the file
    /// using the `include_bytes!()` macro, or you can do fancy things like read it in from
    /// a socket.
    code: &'a [u8],
    /// The name of the program
    name: String,
    /// The name of the program, this value is the argv\[0\] argument to the binary when
    /// executed. If the program expects something specific here, that value should be
    /// used, otherwise any name will do
    program: CString,
    /// The arguments to the program, excluding the program name
    args: Vec<CString>,
    /// The whole argv array, including the program name
    argv: Argv,
    /// The environment variables to set for the program
    env: CommandEnv,
    /// The current working directory to set for the program
    cwd: Option<CString>,
    /// The program's stdin handle
    pub stdin: Option<Stdio>,
    /// The program's stdout handle
    pub stdout: Option<Stdio>,
    /// The program's stderr handle
    pub stderr: Option<Stdio>,
    /// Holdover from Command, whether there was a NUL in the arguments or not
    saw_nul: bool,
}

#[derive(Debug)]
struct Argv(Vec<CString>);

unsafe impl Send for Argv {}
unsafe impl Sync for Argv {}

fn os2c(s: &OsStr, saw_nul: &mut bool) -> CString {
    CString::new(s.as_bytes()).unwrap_or_else(|_e| {
        *saw_nul = true;
        CString::new("<string-with-nul>").unwrap()
    })
}

fn construct_envp(env: BTreeMap<OsString, OsString>, saw_nul: &mut bool) -> Vec<CString> {
    let mut result = Vec::with_capacity(env.len());
    for (mut k, v) in env {
        // Reserve additional space for '=' and null terminator
        k.reserve_exact(v.len() + 2);
        k.push("=");
        k.push(&v);

        // Add the new entry into the array
        if let Ok(item) = CString::new(k.into_vec()) {
            result.push(item);
        } else {
            *saw_nul = true;
        }
    }

    result
}

fn create_and_open_file(path: &Path) -> Result<(File, i32)> {
    let path_dir = path.parent().unwrap();
    create_dir_all(path_dir)?;
    set_permissions(path_dir, Permissions::from_mode(0o700))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .create(true)
        .open(path)?;
    let fd_raw = file.as_raw_fd();
    set_permissions(path, Permissions::from_mode(0o700))?;
    Ok((file, fd_raw))
}

fn do_fexecve(fd: i32, argv: &Vec<&CStr>, envp: &Vec<&CStr>) -> Result<()> {
    let res = fexecve(fd, argv, envp);
    if res.is_err() {
        // If we failed to exec, we need to close the memfd
        // so that the child process doesn't leak it
        unsafe { close(fd) };
        return Err(Error::new(ErrorKind::PermissionDenied, res.err().unwrap()));
    }
    Ok(())
}

fn is_exe(path: &Path) -> bool {
    if let Ok(metadata) = fs::metadata(path) {
        return metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
            && access(path, AccessFlags::X_OK).is_ok()
    }
    false
}

fn try_setsid() {
    if let Err(err) = setsid() {
        eprintln!("Failed to call setsid: {err}");
        exit(1)
    }
}

impl<'a> MemFdExecutable<'a> {
    /// Create a new MemFdExecutable with the given name and code. The name is the name of the
    /// program, and is used as the argv\[0\] argument to the program. The code is the binary
    /// code to execute (usually, the entire contents of an ELF file).
    ///
    /// # Examples
    ///
    /// You can run code that is included directly in your executable with `include_bytes!()`:
    ///
    /// ```no_compile
    /// use memfd_exec::MemFdExecutable;
    ///
    /// let code = include_bytes!("/usr/bin/nc-static");
    ///
    /// let mut cmd = MemFdExecutable::new("nc", code)
    ///     .arg("-l")
    ///     .arg("1234")
    ///     .arg("-e")
    ///     .arg("/bin/sh")
    ///     .status()
    ///     .expect("failed to execute process");
    /// ```
    ///
    pub fn new<S: AsRef<OsStr>>(name: S, code: &'a [u8]) -> Self {
        let mut saw_nul = false;
        let name_cstr = os2c(name.as_ref(), &mut saw_nul);
        Self {
            code,
            name: name.as_ref().to_str().unwrap().to_string(),
            program: name_cstr.clone(),
            args: vec![name_cstr.clone()],
            argv: Argv(vec![name_cstr]),
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
            saw_nul,
        }
    }

    /// Add an argument to the program. This is equivalent to `Command::arg()`.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        let arg = os2c(arg.as_ref(), &mut self.saw_nul);
        self.argv.0.push(arg.clone());
        self.args.push(arg);
        self
    }

    /// Add multiple arguments to the program. This is equivalent to `Command::args()`.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg.as_ref());
        }
        self
    }

    /// Add an environment variable to the program. This is equivalent to `Command::env()`.
    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.env_mut().set(key.as_ref(), val.as_ref());
        self
    }

    /// Add multiple environment variables to the program. This is equivalent to `Command::envs()`.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (ref key, ref val) in vars {
            self.env_mut().set(key.as_ref(), val.as_ref());
        }
        self
    }

    /// Remove an environment variable from the program. This is equivalent to `Command::env_remove()`.
    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
        self.env_mut().remove(key.as_ref());
        self
    }

    /// Clear all environment variables from the program. This is equivalent to `Command::env_clear()`.
    pub fn env_clear(&mut self) -> &mut Self {
        self.env_mut().clear();
        self
    }

    /// Set the current working directory for the program. This is equivalent to `Command::current_dir()`.
    pub fn cwd<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.cwd = Some(os2c(dir.as_ref().as_ref(), &mut self.saw_nul));
        self
    }

    /// Set the stdin handle for the program. This is equivalent to `Command::stdin()`. The
    /// default is to inherit the current process's stdin. Note that this `Stdio` is not the
    /// same exactly as `process::Stdio`, but it is feature-equivalent.
    ///
    /// # Examples
    ///
    /// This example creates a `cat` process that will read in the contents passed to its
    /// stdin handle and write them to a null stdout (i.e. it will be discarded). The same
    /// methodology can be used to read from stderr/stdout.
    ///
    /// ```no_run
    /// use std::thread::spawn;
    /// use std::io::Write;
    ///
    /// use memfd_exec::{MemFdExecutable, Stdio};
    ///
    /// let mut cat_cmd = MemFdExecutable::new("cat", include_bytes!("/bin/cat"))
    ///    .stdin(Stdio::piped())
    ///    .stdout(Stdio::null())
    ///    .spawn()
    ///    .expect("failed to spawn cat");
    ///
    /// let mut cat_stdin = cat_cmd.stdin.take().expect("failed to open stdin");
    /// spawn(move || {
    ///    cat_stdin.write_all(b"hello world").expect("failed to write to stdin");
    /// });
    /// ```
    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.stdin = Some(cfg.into());
        self
    }

    /// Set the stdout handle for the program. This is equivalent to `Command::stdout()`. The
    ///
    /// # Arguments
    /// * `cfg` - The configuration for the stdout handle. This will usually be one of the following:
    ///     * `Stdio::inherit()` - Inherit the current process's stdout handle
    ///     * `Stdio::piped()` - Create a pipe to the child process's stdout. This can be read
    ///     * `Stdio::null()` - Discard all output to stdout
    ///
    /// # Examples
    ///
    /// This example creates a `cat` process that will read in the contents passed to its stdin handle
    /// and read them from its stdout handle. The same methodology can be used to read from stderr/stdout.
    ///
    /// ```
    /// use std::thread::spawn;
    /// use std::fs::read;
    /// use std::io::{Read, Write};
    ///
    /// use memfd_exec::{MemFdExecutable, Stdio};
    ///
    /// let mut cat = MemFdExecutable::new("cat", &read("/bin/cat").unwrap())
    ///     .stdin(Stdio::piped())
    ///     .stdout(Stdio::piped())
    ///     .spawn()
    ///     .expect("failed to spawn cat");
    ///
    /// let mut cat_stdin = cat.stdin.take().expect("failed to open stdin");
    /// let mut cat_stdout = cat.stdout.take().expect("failed to open stdout");
    ///
    /// spawn(move || {
    ///    cat_stdin.write_all(b"hello world").expect("failed to write to stdin");
    /// });
    ///
    /// let mut output = Vec::new();
    /// cat_stdout.read_to_end(&mut output).expect("failed to read from stdout");
    /// assert_eq!(output, b"hello world");
    /// cat.wait().expect("failed to wait on cat");
    /// ```
    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.stdout = Some(cfg.into());
        self
    }

    /// Set the stderr handle for the program. This is equivalent to `Command::stderr()`. The
    ///
    /// # Arguments
    /// * `cfg` - The configuration for the stderr handle. This will usually be one of the following:
    ///    * `Stdio::inherit()` - Inherit the current process's stderr handle
    ///    * `Stdio::piped()` - Create a pipe to the child process's stderr. This can be read
    ///    * `Stdio::null()` - Discard all output to stderr
    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.stderr = Some(cfg.into());
        self
    }

    /// Spawn the program as a child process. This is equivalent to `Command::spawn()`.
    pub fn spawn(&mut self) -> Result<Child> {
        let default = Stdio::Inherit;
        let needs_stdin = true;
        const CLOEXEC_MSG_FOOTER: [u8; 4] = *b"NOEX";

        let envp = self.capture_env();

        if self.saw_nul() {
            // TODO: Need err?
        }

        let (ours, theirs) = self.setup_io(default, needs_stdin)?;

        let (input, output) = anon_pipe()?;

        // Whatever happens after the fork is almost for sure going to touch or
        // look at the environment in one way or another (PATH in `execvp` or
        // accessing the `environ` pointer ourselves). Make sure no other thread
        // is accessing the environment when we do the fork itself.
        //
        // Note that as soon as we're done with the fork there's no need to hold
        // a lock any more because the parent won't do anything and the child is
        // in its own process. Thus the parent drops the lock guard while the child
        // forgets it to avoid unlocking it on a new thread, which would be invalid.
        // TODO: Yeah....I had to remove the env lock. Whoops! Don't multithread env with this
        // you insane person
        let pid = unsafe { self.do_fork()? };

        if pid == 0 {
            drop(input);
            let Err(err) = (unsafe { self.do_exec(theirs, envp) }) else { unreachable!("..."); };
            panic!("failed to exec: {}", err);
        }

        drop(output);

        // Safety: We obtained the pidfd from calling `clone3` with
        // `CLONE_PIDFD` so it's valid an otherwise unowned.
        let mut p = unsafe { Process::new(pid) };
        let mut bytes = [0; 8];

        // loop to handle EINTR
        loop {
            match input.read(&mut bytes) {
                Ok(0) => return Ok(Child::new(p, ours)),
                Ok(8) => {
                    let (errno, footer) = bytes.split_at(4);
                    assert_eq!(
                        CLOEXEC_MSG_FOOTER, footer,
                        "Validation on the CLOEXEC pipe failed: {:?}",
                        bytes
                    );
                    let errno = i32::from_be_bytes(errno.try_into().unwrap());
                    assert!(p.wait().is_ok(), "wait() should either return Ok or panic");
                    return Err(Error::from_raw_os_error(errno));
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    assert!(p.wait().is_ok(), "wait() should either return Ok or panic");
                    panic!("the CLOEXEC pipe failed: {e:?}")
                }
                Ok(..) => {
                    // pipe I/O up to PIPE_BUF bytes should be atomic
                    assert!(p.wait().is_ok(), "wait() should either return Ok or panic");
                    panic!("short read on the CLOEXEC pipe")
                }
            }
        }
    }

    /// Spawn the program as a child process and wait for it to complete, obtaining the
    /// output and exit status. This is equivalent to `Command::output()`.
    pub fn output(&mut self) -> Result<Output> {
        self.spawn()?.wait_with_output()
    }

    /// Spawn the program as a child process and wait for it to complete, obtaining the
    /// exit status. This is equivalent to `Command::status()`.
    pub fn status(&mut self) -> Result<ExitStatus> {
        self.spawn()?.wait()
    }

    /// Set the program name (argv\[0\]) to a new value.
    ///
    /// # Arguments
    /// * `name` - The new name for the program. This will be used as the first argument
    pub fn set_program(&mut self, program: &OsStr) {
        let arg = os2c(program, &mut self.saw_nul);
        self.argv.0[0] = arg.clone();
        self.args[0] = arg;
    }

    fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    fn setup_io(&self, default: Stdio, needs_stdin: bool) -> Result<(StdioPipes, ChildPipes)> {
        let null = Stdio::Null;
        let default_stdin = if needs_stdin { &default } else { &null };
        let stdin = self.stdin.as_ref().unwrap_or(default_stdin);
        let stdout = self.stdout.as_ref().unwrap_or(&default);
        let stderr = self.stderr.as_ref().unwrap_or(&default);
        let (their_stdin, our_stdin) = stdin.to_child_stdio(true)?;
        let (their_stdout, our_stdout) = stdout.to_child_stdio(false)?;
        let (their_stderr, our_stderr) = stderr.to_child_stdio(false)?;
        let ours = StdioPipes {
            stdin: our_stdin,
            stdout: our_stdout,
            stderr: our_stderr,
        };
        let theirs = ChildPipes {
            stdin: their_stdin,
            stdout: their_stdout,
            stderr: their_stderr,
        };
        Ok((ours, theirs))
    }

    fn saw_nul(&self) -> bool {
        self.saw_nul
    }

    /// Get the current working directory for the child process.
    pub fn get_cwd(&self) -> &Option<CString> {
        &self.cwd
    }

    unsafe fn do_fork(&mut self) -> Result<pid_t> {
        cvt(libc::fork())
    }

    fn capture_env(&mut self) -> Option<Vec<CString>> {
        let maybe_env = self.env.capture_if_changed();
        maybe_env.map(|env| construct_envp(env, &mut self.saw_nul))
    }

    /// Execute the command as a new process, replacing the current process.
    ///
    /// This function will not return.
    ///
    /// # Arguments
    /// * `default` - The default stdio to use if the child process does not specify.
    pub fn exec(&mut self, default: Stdio) -> Error {
        let envp = self.capture_env();

        if self.saw_nul() {
            return Error::new(ErrorKind::InvalidInput, "nul byte found in provided data");
        }

        match self.setup_io(default, true) {
            Ok((_, theirs)) => unsafe {
                let Err(e) = self.do_exec(theirs, envp) else { unreachable!("..."); };
                e
            },
            Err(e) => e,
        }
    }

    /// Get the program name to use for the child process as a C string.
    pub fn get_program_cstr(&self) -> &CStr {
        &self.program
    }

    /// Get the program argv to use for the child process.
    pub fn get_argv(&self) -> &Vec<CString> {
        &self.argv.0
    }

    /// Get whether PATH has been affected by changes to the environment variables
    /// of this command.
    pub fn env_saw_path(&self) -> bool {
        self.env.have_changed_path()
    }

    /// Get whether the program (argv\[0\]) is a path, as opposed to a name.
    pub fn program_is_path(&self) -> bool {
        self.program.to_bytes().contains(&b'/')
    }

    fn write_prog<Fd: AsFd>(&self, fd: &Fd) -> Result<()> {
        if let Ok(n) = write(fd, self.code) {
            if n != self.code.len() {
                return Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "Failed to write to memfd",
                ));
            }
        } else {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    fn fallback_exec(&self, argv: &Vec<&CStr>, envp: &Vec<&CStr>) -> Result<()> {
        eprint!(" Trying tmpfile in ");

        let pid = process::id();
        let uid = unsafe { libc::getuid() };

        #[allow(deprecated)]
        for dir in [
            env::temp_dir().to_str().unwrap_or_default(),
            "/dev/shm",
            &format!("{}/.cache", env::home_dir().unwrap_or_default().to_string_lossy())
        ] {
            eprint!("{dir}... ");

            let path_dir = PathBuf::from(format!("{dir}/mfd{uid}{pid}"));
            let path = path_dir.join(&self.name);

            let (file, fd_raw) = create_and_open_file(&path)?;

            if !is_exe(&path) {
                unsafe { close(fd_raw) };
                fs::remove_dir_all(path_dir)?;
                continue
            }
            eprintln!();

            self.write_prog(&file)?;
            unsafe { close(fd_raw) };

            let fd_raw = open(&path, OFlag::O_RDONLY, Mode::empty())?;
            let _ = fs::remove_dir_all(&path_dir);
            let res = do_fexecve(fd_raw, argv, envp);
            
            if res.is_err() {
                let (file, fd_raw) = create_and_open_file(&path)?;

                self.write_prog(&file)?;
                unsafe { close(fd_raw) };
               
                match unsafe { fork() } {
                    Ok(ForkResult::Parent { child: child_pid }) => {
                        spawn(move || waitpid(child_pid, None) );
                        let fd_raw = open(&path, OFlag::O_RDONLY, Mode::empty())?;
                        return do_fexecve(fd_raw, argv, envp)
                    }
                    Ok(ForkResult::Child) => {
                        try_setsid();
                        sleep(Duration::from_millis(2));
                        let _ = fs::remove_dir_all(&path_dir);
                        exit(0)
                    }
                    Err(err) => {
                        eprintln!("fork error: {err}");
                    }
                }
            }
            return res
        }
        Ok(())
    }

    unsafe fn do_exec(
        &mut self,
        stdio: ChildPipes,
        maybe_envp: Option<Vec<CString>>,
    ) -> Result<()> {
        if let Some(fd) = stdio.stdin.fd() {
            cvt_r(|| libc::dup2(fd, libc::STDIN_FILENO))?;
        }
        if let Some(fd) = stdio.stdout.fd() {
            cvt_r(|| libc::dup2(fd, libc::STDOUT_FILENO))?;
        }
        if let Some(fd) = stdio.stderr.fd() {
            cvt_r(|| libc::dup2(fd, libc::STDERR_FILENO))?;
        }

        if let Some(ref cwd) = *self.get_cwd() {
            cvt(libc::chdir(cwd.as_ptr()))?;
        }

        {
            // Reset signal handling so the child process starts in a
            // standardized state. libstd ignores SIGPIPE, and signal-handling
            // libraries often set a mask. Child processes inherit ignored
            // signals and the signal mask from their parent, but most
            // UNIX programs do not reset these things on their own, so we
            // need to clean things up now to avoid confusing the program
            // we're about to run.
            let mut set = MaybeUninit::<libc::sigset_t>::uninit();
            cvt(sigemptyset(set.as_mut_ptr()))?;
            cvt_nz(libc::pthread_sigmask(
                libc::SIG_SETMASK,
                set.as_ptr(),
                null_mut(),
            ))?;

            {
                let ret = signal(libc::SIGPIPE, libc::SIG_DFL);
                if ret == libc::SIG_ERR {
                    return Err(Error::last_os_error());
                }
            }
        }

        // TODO: Env resetting isn't implemented because we're using fexecve not execvp

        let argv = self
            .get_argv()
            .iter()
            .map(|s| s.as_c_str())
            .collect::<Vec<_>>();

        let maybe_envp = maybe_envp.unwrap_or_default();

        let envp = maybe_envp.iter().map(|s| s.as_c_str()).collect::<Vec<_>>();

        if env::var("NO_MEMFDEXEC").unwrap_or_default() == "1" {
            eprint!("memfd-exec is disabled.");
            self.fallback_exec(&argv, &envp)?
        } else {
            // TODO: add detect for qemu emulator
            fn is_running_in_qemu() -> bool {
                true
            }
            let memfd_flags = if is_running_in_qemu() {
                MemFdCreateFlag::empty()
            } else {
                MemFdCreateFlag::MFD_CLOEXEC
            };

            // Map the executable last, because it's a huge hit to memory if something else failed
            let mut mfd_res = memfd_create(
                CString::new(&*self.name).unwrap().as_c_str(),
                memfd_flags,
            );
            if let Ok(mfd) = &mfd_res {
                let mfd_raw = mfd.as_raw_fd();
                if !is_exe(Path::new(&format!("/proc/self/fd/{}", &mfd_raw))) {
                    close(mfd_raw);
                    mfd_res = Err(Errno::EACCES)
                }
            }
            match mfd_res {
                Ok(mfd) => {
                    self.write_prog(&mfd)?;
                    let mut res = do_fexecve(mfd.as_raw_fd(), &argv, &envp);
                    if res.is_err() {
                        eprint!("Failed to exec memfd: {}.", res.unwrap_err());
                        res = self.fallback_exec(&argv, &envp)
                    }
                    return res;
                }
                Err(err) => {
                    eprint!("Failed to create memfd: {err}.");
                    self.fallback_exec(&argv, &envp)?
                }
            }
        }
        Ok(())
    }
}
