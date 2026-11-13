//! Real exec regressions: requires Linux, bubblewrap and a C compiler with static libc.
use std::{
    env, fs,
    io::ErrorKind,
    path::Path,
    process::{Command, Output},
};

use memfd_exec::{MemFdExecutable, Stdio};

#[test]
fn exec_child() {
    let Ok(mode) = env::var("MEMFD_EXEC_CHILD") else {
        return;
    };
    if mode == "denied-memfd" {
        // Deny descriptor exec only, so the same payload can still exec from a file.
        let filter = [
            libc::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 0,
            },
            libc::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 1,
                k: libc::SYS_execveat as u32,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ERRNO | libc::EACCES as u32,
            },
            libc::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ALLOW,
            },
        ];
        let program = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr() as *mut _,
        };
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
            0
        );
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) },
            0
        );
    }
    if mode == "noexec" {
        // Force the first fallback directory to fail the existing X_OK validation.
        assert_eq!(
            unsafe {
                libc::mount(
                    std::ptr::null(),
                    c"/tmp".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | libc::MS_NOEXEC,
                    std::ptr::null(),
                )
            },
            0
        );
    }
    let code = if mode.starts_with("invalid") {
        b"not an executable".to_vec()
    } else {
        fs::read("/payload").unwrap()
    };
    let fd_count = if mode == "invalid-memfd" {
        Some(fs::read_dir("/proc/self/fd").unwrap().count())
    } else {
        None
    };
    let mut executable = MemFdExecutable::new("fixture", &code);
    if mode == "denied-memfd" {
        executable.arg("check-no-memfd");
    }
    let err = executable.exec(Stdio::inherit());
    if let Some(before) = fd_count {
        assert_eq!(
            fs::read_dir("/proc/self/fd").unwrap().count(),
            before,
            "failed exec leaked an fd"
        );
    }
    match mode.as_str() {
        "invalid-memfd" => assert_eq!(err.kind(), ErrorKind::NotADirectory),
        "invalid-fallback" => assert_eq!(err.kind(), ErrorKind::PermissionDenied),
        _ => panic!("exec unexpectedly returned: {err}"),
    }
    println!("exec error returned: {err}");
}

fn run_case(mode: &str, mounts: &[&str]) -> Output {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("fixture.c");
    let payload = work.path().join("fixture");
    fs::write(&source, include_bytes!("fd_ownership.c")).unwrap();
    assert!(Command::new("cc")
        .arg("-static")
        .arg(&source)
        .arg("-o")
        .arg(&payload)
        .status()
        .unwrap()
        .success());

    let mut cmd = Command::new("bwrap");
    cmd.args(["--die-with-parent", "--unshare-net", "--tmpfs", "/"]);
    // Bind only the test runner's dynamic libraries, never the host root or /proc.
    for path in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
        if Path::new(path).exists() {
            cmd.args(["--ro-bind", path, path]);
        }
    }
    cmd.args(mounts)
        .args(["--cap-add", "ALL", "--ro-bind"])
        .arg(env::current_exe().unwrap())
        .arg("/runner")
        .arg("--ro-bind")
        .arg(&payload)
        .arg("/payload")
        .args([
            "--clearenv",
            "--setenv",
            "MEMFD_EXEC_CHILD",
            mode,
            "--setenv",
            "TMPDIR",
        ])
        .arg(if mode == "invalid-memfd" {
            "/payload/not-a-directory"
        } else {
            "/tmp"
        });
    if mode == "disabled" || mode == "noexec" {
        cmd.args(["--setenv", "NO_MEMFDEXEC", "1"]);
    }
    let output = cmd
        .args(["/runner", "--exact", "exec_child", "--nocapture"])
        .output()
        .unwrap();
    eprintln!("case {mode} mounts {mounts:?}: {output:?}");
    output
}

#[test]
fn empty_root_fallback() {
    let output = run_case("success", &["--dev", "/dev"]);
    assert_eq!(output.status.code(), Some(37), "{output:?}");
    assert!(output.stdout.ends_with(b"memfd fixture\n"), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Failed to create memfd for exec: EACCES"),
        "{output:?}"
    );
}

#[test]
fn proc_and_disabled_memfd_controls() {
    for mounts in [&[][..], &["--dev", "/dev"][..]] {
        let output = run_case("success", mounts);
        assert_eq!(output.status.code(), Some(37), "{output:?}");
        assert!(output.stdout.ends_with(b"memfd fixture\n"), "{output:?}");
        let mut with_proc = mounts.to_vec();
        with_proc.extend(["--proc", "/proc"]);
        let output = run_case("success", &with_proc);
        assert_eq!(output.status.code(), Some(37), "{output:?}");
        assert!(output.stdout.ends_with(b"memfd fixture\n"), "{output:?}");
        assert!(
            output.stderr.is_empty(),
            "healthy /proc must use memfd: {output:?}"
        );
        let output = run_case("disabled", &with_proc);
        assert_eq!(output.status.code(), Some(37), "{output:?}");
        assert!(output.stdout.ends_with(b"memfd fixture\n"), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("memfd-exec is disabled."),
            "{output:?}"
        );
    }
}

#[test]
fn failed_fallback_exec_returns_original_error() {
    let output = run_case("invalid-fallback", &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("exec error returned: ENOEXEC"),
        "{output:?}"
    );
}

#[test]
fn noexec_fallback_directory_is_skipped() {
    let output = run_case("noexec", &["--tmpfs", "/tmp", "--tmpfs", "/dev/shm"]);
    assert_eq!(output.status.code(), Some(37), "{output:?}");
    assert!(output.stdout.ends_with(b"memfd fixture\n"), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("/dev/shm..."),
        "{output:?}"
    );
}

#[test]
fn failed_memfd_exec_returns_error() {
    let output = run_case("invalid-memfd", &["--proc", "/proc"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("exec error returned:"),
        "{output:?}"
    );
}

#[test]
fn failed_memfd_is_closed_before_successful_fallback() {
    let output = run_case("denied-memfd", &["--proc", "/proc"]);
    assert_eq!(output.status.code(), Some(37), "{output:?}");
    assert!(output.stdout.ends_with(b"memfd fixture\n"), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Failed to exec memfd: EACCES"),
        "{output:?}"
    );
}
