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
    let mut cmd = command_with(program, args);
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
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

/// A command that runs `program`. Windows can't start a `#!` script itself (a file with no extension
/// that starts `#!`, such as a wrapper config.toml names for `gh` or `claude`, or a test's fake), so
/// there it runs in Git Bash: `bash <script> <args>`. It opens no console window ([`no_window`]).
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[cfg(windows)]
    if is_sh_script(Path::new(program.as_ref())) {
        return git_bash_with([program.as_ref()]);
    }
    let mut c = Command::new(program);
    no_window(&mut c);
    c
}

/// On Windows, a console program started by a process with no console (the app, or a `tb` started
/// detached) gets a console window of its own, which flashes up for as long as it runs. This starts
/// it with a hidden one instead. A process with a console, shown or hidden (`tb` in a terminal, the
/// daemon), shares it with what it starts, as before. Elsewhere it does nothing.
pub fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    if !has_console() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// This process is attached to a console, hidden or not. (`GetConsoleWindow` can't tell: a hidden one
/// has no window either.)
#[cfg(windows)]
fn has_console() -> bool {
    // SAFETY: no arguments; it fails, answering 0, when there's no console.
    unsafe { windows_sys::Win32::System::Console::GetConsoleCP() != 0 }
}

/// `command(program)` with `args`, written the way the program reads them: through [`git_bash_with`]
/// when it runs in Git Bash.
pub fn command_with<I, S>(program: impl AsRef<std::ffi::OsStr>, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    #[cfg(windows)]
    {
        let p = Path::new(program.as_ref());
        let script = is_sh_script(p);
        if script || p == git_bash() {
            let first = script.then(|| program.as_ref().to_os_string());
            return git_bash_with(first.into_iter().chain(args.into_iter().map(|a| a.as_ref().to_os_string())));
        }
    }
    let mut cmd = command(program.as_ref());
    cmd.args(args);
    cmd
}

/// The longest argument [`git_bash_with`] puts on Git Bash's command line, in UTF-8 bytes. Cygwin keeps
/// only the first 8186 characters of a quoted one and drops the rest without a word.
#[cfg(windows)]
const QUOTED_ARG_MAX: usize = 8000;

/// What [`git_bash_with`] names the environment variables an argument too long for the command line
/// goes in: `TASKBOARD_ARGC`, then `TASKBOARD_ARG_1` and on.
#[cfg(windows)]
const ARG_VAR: &str = "TASKBOARD_ARG";

/// Puts `"$@"` back together from the `TASKBOARD_ARG_<n>` variables, unsets them, and runs bash with it.
#[cfg(windows)]
const ARGS_FROM_ENV: &str = r#"i=1; set --; while [ "$i" -le "$TASKBOARD_ARGC" ]; do eval "set -- \"\$@\" \"\$TASKBOARD_ARG_$i\""; unset "TASKBOARD_ARG_$i"; i=$((i+1)); done; unset TASKBOARD_ARGC i; exec "$BASH" "$@""#;

/// Git Bash run with `args` (`sh()`, `shell`, a script `command` runs), opening no console window
/// ([`no_window`]). Git Bash reads its command line the way Cygwin does, not the way Rust writes it: in
/// quotes `\\` is one backslash, and outside them `'` quotes, `*`, `{a,b}` and `~` expand and `@file`
/// reads a file. So each argument goes in double quotes with `\` and `"` escaped, every other character
/// taken as it is. One that starts with a drive (`C:`) keeps its backslashes as written once inside
/// quotes, so its letter goes before them. Cygwin cuts a quoted argument short, though
/// ([`QUOTED_ARG_MAX`]), so when one is longer (a review's prompt to a `#!` wrapper of `claude`) every
/// argument goes in an environment variable instead and a short `bash -c` ([`ARGS_FROM_ENV`]) runs bash
/// with them.
#[cfg(windows)]
fn git_bash_with<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    use std::os::windows::process::CommandExt;
    let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_string_lossy().into_owned()).collect();
    let mut c = Command::new(git_bash());
    no_window(&mut c);
    if args.iter().all(|a| a.len() <= QUOTED_ARG_MAX) {
        for a in &args {
            c.raw_arg(cygwin_quoted(a));
        }
        return c;
    }
    c.raw_arg(cygwin_quoted("-c")).raw_arg(cygwin_quoted(ARGS_FROM_ENV));
    c.env(format!("{ARG_VAR}C"), args.len().to_string());
    for (i, a) in args.iter().enumerate() {
        c.env(format!("{ARG_VAR}_{}", i + 1), a);
    }
    c
}

/// `arg` written for a Cygwin program's command line ([`git_bash_with`]).
#[cfg(windows)]
fn cygwin_quoted(arg: &str) -> String {
    let b = arg.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let (head, rest) = arg.split_at(if drive { 1 } else { 0 });
    let mut out = String::with_capacity(arg.len() + 4);
    out.push_str(head);
    out.push('"');
    for c in rest.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `p` is a script given by its path (not a bare name looked up on PATH) whose file starts `#!` and
/// has no extension: one Windows runs through Git Bash (`command`).
fn is_sh_script(p: &Path) -> bool {
    let by_path = p.parent().is_some_and(|d| !d.as_os_str().is_empty());
    let mut head = [0u8; 2];
    by_path && p.extension().is_none() && std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"#!"
}

/// Lets `path` run as a program: executable on Unix. Nothing on Windows, where a `#!` script runs
/// through Git Bash (`command`) and any other program runs by its extension.
pub fn make_runnable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

/// A command that runs `script` in a shell: `/bin/sh -c` on Unix, Git Bash's `bash -c` on Windows, so
/// the same POSIX commands in config.toml run on both.
pub fn shell(script: &str) -> Command {
    #[cfg(windows)]
    return git_bash_with(["-c", script]);
    #[cfg(unix)]
    {
        let mut c = Command::new(sh());
        c.args(["-c", script]);
        no_window(&mut c);
        c
    }
}

/// The shell config.toml's commands run in (`sh -c "…"`): `/bin/sh` on Unix, Git Bash on Windows.
pub fn sh() -> std::path::PathBuf {
    #[cfg(unix)]
    return std::path::PathBuf::from("/bin/sh");
    #[cfg(windows)]
    return git_bash();
}

/// Git Bash's `bash.exe`, which Claude Code on Windows needs too: `CLAUDE_CODE_GIT_BASH_PATH` (the
/// variable Claude Code reads), else the one beside the `git.exe` on PATH, else Git's default install.
/// Looked for once.
#[cfg(windows)]
pub fn git_bash() -> std::path::PathBuf {
    static FOUND: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    FOUND.get_or_init(find_git_bash).clone()
}

#[cfg(windows)]
fn find_git_bash() -> std::path::PathBuf {
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

/// Windows: the named event that asks `taskboardd serve` (process `pid`) to stop cleanly, as
/// launchd's SIGTERM does on macOS. Its supervisor (`taskboard-app --daemon`) sets it. Not a
/// console ctrl-break: that would reach every program the daemon runs in its console too.
#[cfg(windows)]
pub fn stop_event(pid: u32) -> String {
    format!(r"Local\taskboardd.stop.{pid}")
}

/// Windows: calls `f`, on a thread of its own, once this process's [`stop_event`] is set. False
/// when the event couldn't be made, so nothing can ask this run to stop that way.
#[cfg(windows)]
pub fn on_stop_asked(f: impl FnOnce() + Send + 'static) -> bool {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    let name: Vec<u16> = stop_event(std::process::id()).encode_utf16().chain(Some(0)).collect();
    // Manual reset. One the supervisor made first (a stop asked while this was starting) opens as
    // it is, set or not.
    let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    if h.is_null() {
        return false;
    }
    // Held for the life of the process. It crosses to the thread as a number (a raw pointer isn't Send).
    let h = h as usize;
    std::thread::Builder::new()
        .name("stop-event".into())
        .spawn(move || {
            if unsafe { WaitForSingleObject(h as _, INFINITE) } == WAIT_OBJECT_0 {
                f();
            }
        })
        .is_ok()
}

/// Starts a command the board doesn't wait for, and reaps it once it exits so it isn't left a
/// zombie under the board (a `Child` dropped without a wait is never reaped).
pub fn start_reaped(cmd: &mut Command) -> std::io::Result<()> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Finds a command on PATH (or returns the path as given when it has a slash). On Windows a name
/// without an extension is looked for with each of PATHEXT's (`git` is `git.exe`, `npm` is `npm.cmd`);
/// a path to a `#!` script with none beside it is that script (`command` runs it in Git Bash).
pub fn which(name: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') || (cfg!(windows) && name.contains('\\')) {
        let p = crate::util::expand_home(name);
        return runnable_as(&p).or_else(|| is_sh_script(&p).then_some(p));
    }
    let path = std::env::var_os("PATH")?;
    let extra: &[&str] = if cfg!(windows) { &["~/.local/bin"] } else { &["~/.local/bin", "/opt/homebrew/bin", "/usr/local/bin"] };
    std::env::split_paths(&path).chain(extra.iter().map(|d| crate::util::expand_home(d))).find_map(|dir| runnable_as(&dir.join(name)))
}

/// `p` when it's a file. On Windows, when `p` has no extension, `p` with the first of PATHEXT's that
/// names a file instead: a file without one isn't a program there (npm keeps a shell script called
/// `npm` beside `npm.cmd`).
fn runnable_as(p: &std::path::Path) -> Option<std::path::PathBuf> {
    if !cfg!(windows) || p.extension().is_some() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    exts.split(';').filter(|e| !e.is_empty()).find_map(|e| {
        let mut s = p.as_os_str().to_owned();
        s.push(e.to_ascii_lowercase());
        let c = std::path::PathBuf::from(s);
        c.is_file().then_some(c)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app has no console, and what it starts mustn't flash one up. The test runs a copy of itself
    /// detached (no console), which starts another through `command`; that one says whether it got a
    /// console window.
    #[cfg(windows)]
    #[test]
    fn a_program_started_by_one_with_no_console_opens_no_window() {
        const ROLE: &str = "TB_NO_WINDOW_ROLE";
        const OUT: &str = "TB_NO_WINDOW_OUT";
        let me = std::env::current_exe().unwrap();
        let this = ["proc::tests::a_program_started_by_one_with_no_console_opens_no_window", "--exact"];
        match std::env::var(ROLE).as_deref() {
            Ok("inner") => {
                // SAFETY: no arguments; null when the console has no window, or there's no console.
                let window = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
                std::fs::write(std::env::var(OUT).unwrap(), if window.is_null() { "no window" } else { "a window" }).unwrap();
            }
            Ok("outer") => {
                assert!(!has_console());
                command(&me).args(this).env(ROLE, "inner").stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
            }
            _ => {
                assert!(has_console(), "a test run has a console");
                let out = std::env::temp_dir().join(format!("tb-no-window-{}", std::process::id()));
                let _ = std::fs::remove_file(&out);
                let mut outer = Command::new(&me);
                outer.args(this).env(ROLE, "outer").env(OUT, &out).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                detach(&mut outer).status().unwrap();
                let got = std::fs::read_to_string(&out).unwrap_or_else(|e| panic!("the copy started with no console didn't report: {e}"));
                let _ = std::fs::remove_file(&out);
                assert_eq!(got, "no window");
            }
        }
    }

    #[test]
    fn which_finds_git() {
        let git = which("git").expect("git on the PATH");
        assert!(git.is_file());
        assert_eq!(which(&git.to_string_lossy()), Some(git.clone()), "a path is taken as given");
        if cfg!(windows) {
            assert!(git.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")), "{}", git.display());
        }
        assert_eq!(which("no-such-program-anywhere"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_program_is_the_file_with_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        // npm's layout: a shell script with no extension beside the .cmd Windows runs.
        std::fs::write(dir.path().join("tool"), "#!/bin/sh\n").unwrap();
        assert_eq!(runnable_as(&dir.path().join("tool")), None);
        let path = dir.path().join("tool").to_string_lossy().to_string();
        assert_eq!(which(&path), Some(dir.path().join("tool")), "by its path, a lone script runs in Git Bash");
        std::fs::write(dir.path().join("tool.cmd"), "@echo off\n").unwrap();
        assert_eq!(runnable_as(&dir.path().join("tool")), Some(dir.path().join("tool.cmd")));
        assert_eq!(runnable_as(&dir.path().join("tool.cmd")), Some(dir.path().join("tool.cmd")));
        assert_eq!(which(&path), Some(dir.path().join("tool.cmd")), "the .cmd beside it comes first");
    }

    #[test]
    fn a_sh_script_runs_by_its_path_on_every_platform() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake");
        std::fs::write(&fake, "#!/bin/sh\necho \"said $1 and $2\"\nexit 3\n").unwrap();
        make_runnable(&fake).unwrap();
        let args = ["one".to_string(), r#"{"id": "C:\\x y"}"#.to_string()];
        let o = run(&fake, &args, None, 10.0).ok().expect("it ran");
        assert_eq!((o.code, o.stdout.trim()), (Some(3), r#"said one and {"id": "C:\\x y"}"#));
        assert!(!is_sh_script(Path::new("fake")), "a bare name is looked up, not read from here");
    }

    /// Words Git Bash's own command-line reading would change if Rust's quoting reached it.
    const AWKWARD: &[&str] = &[
        r#"{"id": "C:\\x y"}"#,
        r"C:\Users\me\a b",
        r#"C:\x "quoted" \\two\\"#,
        "it's",
        "*.rs",
        "{a,b}",
        "~/x",
        "@args",
        r"\\server\share\",
        r#"ends with \"#,
        "",
        "{}",
        "naïve ünïcode",
        "two\nlines",
    ];

    #[test]
    fn git_bash_gets_every_argument_as_written() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), "").unwrap();
        let args: Vec<String> = AWKWARD.iter().map(|s| s.to_string()).collect();
        let script = r#"for a in "$@"; do printf '[%s]' "$a"; done"#;
        let mut all = vec!["-c".to_string(), script.to_string(), "sh".to_string()];
        all.extend(args.iter().cloned());
        let want: String = AWKWARD.iter().map(|a| format!("[{a}]")).collect();
        let o = run_with(&sh(), &all, Some(dir.path()), 10.0, &[], None).ok().expect("it ran");
        assert_eq!(o.stdout, want, "{}", o.stderr);
        let o = shell(&format!("printf '[%s]' {}", r#""C:\\x \\\\y""#)).current_dir(dir.path()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout), r"[C:\x \\y]", "a script's own backslashes");
        let fake = dir.path().join("fake");
        std::fs::write(&fake, format!("#!/bin/sh\n{script}\n")).unwrap();
        make_runnable(&fake).unwrap();
        let o = run(&fake, &args, Some(dir.path()), 10.0).ok().expect("it ran");
        assert_eq!(o.stdout, want, "{}", o.stderr);
    }

    #[test]
    fn git_bash_gets_a_long_argument_whole() {
        let dir = tempfile::tempdir().unwrap();
        // Past where Cygwin cuts a quoted argument off, with every awkward word in it.
        let long: String = AWKWARD.iter().cycle().take(1500).map(|a| format!("{a} ")).collect();
        assert!(long.len() > 10_000);
        let args = vec!["first".to_string(), long.clone(), "".to_string(), "last".to_string()];
        let script = r#"printf '%s|' "$#" "$1"; printf %s "$2" > long.txt; printf '|%s|%s' "$3" "$4""#;
        let fake = dir.path().join("fake");
        std::fs::write(&fake, format!("#!/bin/sh\n{script}\n")).unwrap();
        make_runnable(&fake).unwrap();
        let o = run(&fake, &args, Some(dir.path()), 10.0).ok().expect("it ran");
        assert_eq!(o.stdout, "4|first|||last", "{}", o.stderr);
        assert!(std::fs::read_to_string(dir.path().join("long.txt")).unwrap() == long, "the long one, every character");
        let mut all = vec!["-c".to_string(), script.to_string(), "sh".to_string()];
        all.extend(args.iter().cloned());
        let o = run_with(&sh(), &all, Some(dir.path()), 10.0, &[], None).ok().expect("it ran");
        assert_eq!(o.stdout, "4|first|||last", "Git Bash run directly: {}", o.stderr);
        assert!(std::fs::read_to_string(dir.path().join("long.txt")).unwrap() == long);
        let o = shell(&format!("{script}; printf '|%s' \"${{TASKBOARD_ARG_2-none}}\"\n: <<'END'\n{long}\nEND\n")).current_dir(dir.path()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout), "0|||||none", "a long script runs, and the variables are gone");
    }

    /// The only test that asks this process to stop: the event stays set for the rest of the run.
    #[cfg(windows)]
    #[test]
    fn setting_the_stop_event_reaches_the_process_it_names() {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(on_stop_asked(move || tx.send(()).unwrap()));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "not asked yet");
        let name: Vec<u16> = stop_event(std::process::id()).encode_utf16().chain(Some(0)).collect();
        let ev = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        assert!(!ev.is_null(), "the event is there to open by the process's id");
        unsafe { SetEvent(ev) };
        unsafe { CloseHandle(ev) };
        rx.recv_timeout(Duration::from_secs(5)).expect("asked within 5 seconds");
    }

    // Zombies are Unix's: Windows keeps no exited child waiting to be reaped.
    #[cfg(unix)]
    #[test]
    fn a_started_command_is_reaped_once_it_exits() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo $$ > \"$0\"", "pidfile"]);
        let dir = tempfile::tempdir().unwrap();
        cmd.current_dir(dir.path());
        start_reaped(&mut cmd).ok().unwrap();
        let file = dir.path().join("pidfile");
        let until = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(p) = std::fs::read_to_string(&file).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
                break p;
            }
            assert!(Instant::now() < until, "the command never ran");
            std::thread::sleep(Duration::from_millis(20));
        };
        // Reaped: the process is gone, not a zombie still waiting for the board to wait on it.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < until, "process {pid} is still there (a zombie)");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
