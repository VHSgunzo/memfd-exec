# memfd_exec ![crates.io](https://img.shields.io/crates/v/memfd-exec.svg)

This is a very simple crate that allows execution of in-memory only programs. Simply
put, if you have the contents of a Linux executable in a `Vec<u8>`, you can use
`memfd_exec` to execute the program without it ever touching your hard disk. Use
cases for this may include:

* Bundling a static executable with another program (for example, my motivation to
  create this package is that I want to ship a statically built QEMU with
  [cantrace](https://github.com/novafacing/cannoli))
* Sending executables over the network and running them, to reduce footprint and increase
  throughput
* Really hacky stuff that I haven't thought of, if you have a cool use case, feel free
  to make a PR to the README or add an example in [examples](examples)

## Using

Just include `memfd-exec = "0.2.5"` in your `Cargo.toml` file.

## Features

* Feature-parity API with `process::Command`, the only difference is we don't execute
  anything from disk.
* Only two dependencies

## Examples

### Run an executable downloaded over the network

For redteamers, this example will download and run an executable without ever writing it
to disk. It may not bypass Advanced Threat Protection, but it at least won't leave
a huge disk footprint!

```rust
use memfd_exec::{MemFdExecutable, Stdio};
use reqwest::blocking::get;

const URL: &str = "https://novafacing.github.io/assets/qemu-x86_64";
let resp = get(URL).unwrap();

// The `MemFdExecutable` struct is at near feature-parity with `std::process::Command`,
// so you can use it in the same way. The only difference is that you must provide the
// argv[0] to use as well as the executable contents as a byte slice.
let qemu = MemFdExecutable::new("qemu-x86_64", resp.bytes().unwrap().to_vec())
    // We'll just get the version here, but you can do anything you want with the
    // args.
    .arg("-version")
    // We'll capture the stdout of the process, so we need to set up a pipe.
    .stdout(Stdio::piped())
    // Spawn the process as a forked child
    .spawn()
    .unwrap();

// Get the output and status code of the process (this will block until the process
// exits)
let output = qemu.wait_with_output().unwrap();
assert!(output.status.into_raw() == 0);
// Print out the version we got!
println!("{}", String::from_utf8_lossy(&output.stdout));
```

### Bundle and run a local static executable

The motivating example for this project is to bundle an executable along with a rust
program and be able to run the executable straight from memory instead of going
through the tedious and slow process of writing the executable file to disk and then
invoking it as a command.

This example creates an executable with a bundled [program](tests/test_static.c) that
opens a socket, reads a bit of input, and then prints out the input. Of course, the
logical extension of the idea would be to use a static
[netcat](https://github.com/openbsd/src/blob/master/usr.bin/nc/netcat.c) build or some
such thing.

```rust

use memfd_exec::{MemFdExecutable, Stdio};

const EXECUTABLE_FILE: &[u8] = include_bytes!("tets/test_static");

fn main() {
    const PORT = 1234;
    // We create an in-memory executable with an argv[0] "test" and an executable file
    // that we embedded in our rust binary
    let exe = MemFdExecutable::new("test", EXECUTABLE_FILE.to_vec())
        // We pass one arg, the port number to listen on
        .arg(format!("{}", PORT))
        // We tell it to use a pipe for stdout (stdin and stderr will default to Stdio::inherit())
        .stdout(Stdio::piped())
        // We spawn the child process as a forked child process
        .spawn()
        .expect("Failed to create process!");

    // Wait until the process finishes and print its output
    let output = exe.wait_with_output().unwrap();
    println!("Got output: {:?}", output.stdout);
}
```

## Testing

For testing purposes, you need to install:

- `clang`
- `glibc-static`
- `glibc`
- `bubblewrap` (`bwrap`), with user/mount namespace support, for `fd_ownership`
- `cc`, with static libc support, for the tiny `fd_ownership` payload

Run the descriptor-ownership regressions with:

```sh
cargo test --locked --test fd_ownership -- --nocapture
```

These tests re-execute the test runner in a private empty root (binding only its
loader/libraries and payload), with and without `/proc` and `/dev`. They check
normal memfd execution, the existing on-disk fallback when `/proc` is absent or
`NO_MEMFDEXEC=1`, a noexec fallback directory, and returned exec errors without
double-closes or leaked failed-exec memfds (including seccomp-denied descriptor
exec followed by successful file exec). The fallback policy is unchanged: absent `/proc`
still selects a temporary file, not direct `execveat` execution. No host mounts
are modified; the noexec mount belongs only to the bubblewrap sandbox.

You should also have `/bin/cat` and `/bin/ls` on your system. This is default on the
vast majority of Linux systems, but don't panic if this test fails if they are missing.
