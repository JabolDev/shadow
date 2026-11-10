//! What `shadow run` asks of the system so that it owns its output and its
//! processes:
//! - stderr (fd 2) is a pipe; one thread writes what comes through it to the
//!   real stderr with each key value replaced by `[redacted]`. When the real
//!   stderr fails, it keeps reading (no writer fails or blocks) and says so
//!   once, through `broken`.
//! - the run is a child subreaper: a process its commands leave behind becomes
//!   its child, so at the end it can end them by their own pids.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub struct Stderr {
    real: OwnedFd,
    pipe: OwnedFd,
    thread: JoinHandle<()>,
    pub broken: watch::Receiver<bool>,
}

/// Puts the redacting pipe in place of fd 2.
pub fn redact_stderr(keys: Vec<String>) -> std::io::Result<Stderr> {
    let mut fds = [0; 2];
    // SAFETY: pipe2(2) writes two new descriptors into the array it is given;
    // F_DUPFD_CLOEXEC and dup2(2) only take and return descriptor numbers.
    let (read, pipe, real) = unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (read, pipe) = (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]));
        let real = match libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3) {
            -1 => OwnedFd::from(File::options().write(true).open("/dev/null")?),
            fd => OwnedFd::from_raw_fd(fd),
        };
        if libc::dup2(pipe.as_raw_fd(), 2) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        (read, pipe, real)
    };
    let (tell, broken) = watch::channel(false);
    let mut out = File::from(real.try_clone()?);
    let thread = std::thread::spawn(move || {
        let (mut from, mut buf, mut held) = (File::from(read), [0; 8192], Vec::new());
        let mut write = |bytes: &[u8]| {
            if !*tell.borrow() && out.write_all(&redact(bytes, &keys)).is_err() {
                tell.send_replace(true);
            }
        };
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            held.extend_from_slice(&buf[..n]);
            let cut = ready(&held, &keys);
            if cut > 0 {
                let rest = held.split_off(cut);
                write(&held);
                held = rest;
            }
        }
        write(&held);
    });
    Ok(Stderr {
        real,
        pipe,
        thread,
        broken,
    })
}

impl Stderr {
    /// Puts the real stderr back and waits until everything written got out.
    /// False when the real stderr failed.
    pub fn finish(self) -> bool {
        // SAFETY: dup2(2) only takes descriptor numbers; fd 2 stops being the pipe.
        unsafe { libc::dup2(self.real.as_raw_fd(), 2) };
        drop(self.pipe);
        // The thread ends when the pipe is closed; a panic there was already reported.
        let _ = self.thread.join();
        !*self.broken.borrow()
    }
}

/// How much of `held` can go out now: up to the last line end, or for a
/// line longer than 64 KB all but a tail that could be the start of a key;
/// never through the middle of a key.
fn ready(held: &[u8], keys: &[String]) -> usize {
    let keep = keys
        .iter()
        .map(String::len)
        .max()
        .unwrap_or(1)
        .saturating_sub(1);
    let mut cut = match held.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None if held.len() > 1 << 16 => held.len() - keep,
        None => return 0,
    };
    loop {
        let across = keys.iter().filter(|k| !k.is_empty()).flat_map(|k| {
            let k = k.as_bytes();
            (cut.saturating_sub(k.len() - 1)..cut)
                .filter(move |&s| s + k.len() > cut && held[s..].starts_with(k))
        });
        match across.min() {
            Some(start) => cut = start,
            None => return cut,
        }
    }
}

/// `text` with every key value replaced by `[redacted]`.
pub fn redact(text: &[u8], keys: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        match keys
            .iter()
            .find(|k| !k.is_empty() && text[i..].starts_with(k.as_bytes()))
        {
            Some(k) => {
                out.extend_from_slice(b"[redacted]");
                i += k.len();
            }
            None => {
                out.push(text[i]);
                i += 1;
            }
        }
    }
    out
}

/// Makes the run the reaper of every orphan in its process tree.
#[cfg(target_os = "linux")]
pub fn become_subreaper() -> std::io::Result<()> {
    // SAFETY: prctl(2) with PR_SET_CHILD_SUBREAPER only reads its integer arguments.
    match unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// The run's child processes: (pid, process group, is a zombie).
fn children() -> Vec<(i32, i32, bool)> {
    let me = std::process::id().to_string();
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // After the name: state, parent, process group, …
        let f: Vec<&str> = stat
            .rsplit_once(')')
            .map_or(Vec::new(), |(_, rest)| rest.split_whitespace().collect());
        if f.len() > 2
            && f[1] == me
            && let Ok(group) = f[2].parse()
        {
            out.push((pid, group, f[0] == "Z"));
        }
    }
    out
}

/// Reaps orphans that have ended, while the run goes on. Only processes that
/// lead no process group: every command the engine starts leads its own, and
/// the engine reaps those itself.
pub fn reap_orphans() {
    for (pid, group, zombie) in children() {
        if zombie && pid != group {
            // SAFETY: waitpid(2) on our own ended child; a null status pointer is allowed.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        }
    }
}

/// At the end, with no other reaper left: TERM to every child, KILL after
/// 0.5 s, and reap, until the run has no child. A child's pid cannot be
/// reused before it is reaped, so each signal reaches the process meant.
pub fn end_children() {
    let start = Instant::now();
    let mut termed = std::collections::HashSet::new();
    loop {
        let kids = children();
        if kids.is_empty() {
            return;
        }
        if start.elapsed() > Duration::from_secs(5) {
            eprintln!("shadow: {} processes did not end", kids.len());
            return;
        }
        let late = start.elapsed() > Duration::from_millis(500);
        for (pid, _, zombie) in kids {
            // SAFETY: waitpid(2) and kill(2) on our own unreaped child, by its pid.
            unsafe {
                if zombie {
                    libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
                } else if late {
                    libc::kill(pid, libc::SIGKILL);
                } else if termed.insert(pid) {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
#[path = "cli_os_test.rs"]
mod tests;
