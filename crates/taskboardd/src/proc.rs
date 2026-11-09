//! Running a command with a time limit.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::slowlog::{self, Ended};

pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub enum RunError {
    Spawn(std::io::Error),
    TimedOut,
}

pub fn run(program: &Path, args: &[String], cwd: Option<&Path>, timeout: f64) -> Result<Output, RunError> {
    run_with(program, args, cwd, timeout, &[], None)
}

/// `run` with extra environment variables and, optionally, bytes written to the command's stdin.
pub fn run_with(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    timeout: f64,
    env: &[(String, String)],
    input: Option<&[u8]>,
) -> Result<Output, RunError> {
    run_until(program, args, cwd, timeout, env, input, &|| false).map(|o| o.expect("never stopped"))
}

/// `run_with` that kills the command when `stop` says so (asked about once a second): `None` then.
pub fn run_until(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    timeout: f64,
    env: &[(String, String)],
    input: Option<&[u8]>,
    stop: &dyn Fn() -> bool,
) -> Result<Option<Output>, RunError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        let bytes = bytes.to_vec();
        // A command that never reads its stdin mustn't hold up the time limit.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let mut out = child.stdout.take().unwrap();
    let mut errp = child.stderr.take().unwrap();
    let (otx, orx) = std::sync::mpsc::channel();
    let (etx, erx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        let _ = otx.send(s);
    });
    std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = errp.read_to_end(&mut s);
        let _ = etx.send(s);
    });
    let end = Instant::now() + Duration::from_secs_f64(timeout.max(0.1));
    let mut asked = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                // Whatever it had said by then; a helper holding the pipe open gets only a moment.
                let stderr = String::from_utf8_lossy(&erx.recv_timeout(Duration::from_millis(200)).unwrap_or_default()).into_owned();
                slowlog::command(program, args, cwd, started.elapsed(), Ended::TimedOut { stderr: &stderr });
                return Err(RunError::TimedOut);
            }
            Ok(None) if asked.elapsed() >= Duration::from_secs(1) && {
                asked = Instant::now();
                stop()
            } =>
            {
                let _ = child.kill();
                let _ = child.wait();
                slowlog::command(program, args, cwd, started.elapsed(), Ended::Exited { code: None, stderr: "stopped" });
                return Ok(None);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => break None,
        }
    };
    // A helper process the command left behind can hold its pipes open; don't wait on it for long.
    let grace = Duration::from_secs(2);
    let stdout = String::from_utf8_lossy(&orx.recv_timeout(grace).unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&erx.recv_timeout(grace).unwrap_or_default()).into_owned();
    let code = status.and_then(|s| s.code());
    slowlog::command(program, args, cwd, started.elapsed(), Ended::Exited { code, stderr: &stderr });
    Ok(Some(Output { code, stdout, stderr }))
}

/// A command that runs `script` in a shell: `/bin/sh -c` on Unix, Git Bash's `bash -c` on Windows, so
/// the same POSIX commands in config.toml run on both.
pub fn shell(script: &str) -> Command {
    #[cfg(unix)]
    let mut c = Command::new("/bin/sh");
    #[cfg(windows)]
    let mut c = Command::new(git_bash());
    c.args(["-c", script]);
    c
}

/// Git Bash's `bash.exe`, which Claude Code on Windows needs too: `CLAUDE_CODE_GIT_BASH_PATH` (the
/// variable Claude Code reads), else the one beside the `git.exe` on PATH, else Git's default install.
#[cfg(windows)]
pub fn git_bash() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH").map(std::path::PathBuf::from).filter(|p| p.is_file()) {
        return p;
    }
    // `<Git>\cmd\git.exe`, `<Git>\bin\git.exe` or `<Git>\mingw64\bin\git.exe`.
    if let Some(git) = which("git.exe") {
        if let Some(b) = git.ancestors().skip(1).take(3).map(|d| d.join("bin").join("bash.exe")).find(|b| b.is_file()) {
            return b;
        }
    }
    std::path::PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")
}

/// Starts `cmd` on its own, so whatever ends the caller's group (Claude ending a hook's) doesn't end
/// it: its own process group on Unix; on Windows its own console process group, with no console.
pub fn detach(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd
}

/// A child and whatever it starts, so a timeout or a stop ends them all: on Unix the child leads its
/// own process group, on Windows it's put in a job object. `prepare` the command, spawn it, then take
/// the child's `Group`.
pub struct Group {
    #[cfg(unix)]
    pgid: i32,
    /// Null when the job couldn't be made or the child couldn't join it; `kill` then does nothing,
    /// so callers kill the child itself as well.
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl Group {
    pub fn prepare(cmd: &mut Command) -> &mut Command {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd
    }

    #[cfg(unix)]
    pub fn of(child: &std::process::Child) -> Group {
        Group { pgid: child.id() as i32 }
    }

    /// The child's new job. What the child starts in the moment before it joins isn't in the job.
    #[cfg(windows)]
    pub fn of(child: &std::process::Child) -> Group {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        // SAFETY: an unnamed job with default security; the child's handle is open while it's borrowed.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Group { job };
        }
        if unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0 {
            // SAFETY: the job was just made and nothing else holds it.
            unsafe { CloseHandle(job) };
            return Group { job: std::ptr::null_mut() };
        }
        Group { job }
    }

    /// Kills every process in the group, the child too if it's still running.
    pub fn kill(&self) {
        // SAFETY: plain syscalls on the group's own pgid / job handle.
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pgid, libc::SIGKILL);
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
    }
}

#[cfg(windows)]
impl Drop for Group {
    fn drop(&mut self) {
        if !self.job.is_null() {
            // SAFETY: the group owns its job handle and nothing uses it after this.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}

/// Finds a command on PATH (or returns the path as given when it has a slash).
pub fn which(name: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') {
        let p = crate::util::expand_home(name);
        return if p.is_file() { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    for extra in ["~/.local/bin", "/opt/homebrew/bin", "/usr/local/bin"] {
        let p = crate::util::expand_home(extra).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}
