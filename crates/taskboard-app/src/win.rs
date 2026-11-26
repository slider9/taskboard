//! Windows: what an installed app sets up for itself, where macOS has the bundle, launchd and
//! LaunchServices.
//!
//! - **The install** (`%LOCALAPPDATA%\Programs\Taskboard`, which the release folder
//!   `packaging/windows/build.sh` makes copies itself into: `win_install.rs`) holds the three
//!   programs, the plugin, the marker and [`MANIFEST`]: the install's id, name, link scheme and
//!   daemon label, and the environment its app and daemon run with. That's what Info.plist (with
//!   its LSEnvironment) and the LaunchAgent's plist say on macOS; Taskboard Dev sets its own data
//!   folder and port there.
//! - **The daemon** runs under `taskboard-app.exe --daemon` ([`supervise`]), which a per-user Run
//!   entry starts at sign-in. It starts `taskboardd.exe serve` without a window and starts it again
//!   after a crash, at most every 10 seconds, but not after a clean exit (another board holds the
//!   port), like launchd's KeepAlive. [`stop`] signals the supervisor's stop event, and it asks the
//!   daemon to stop (the daemon's own stop event, `taskboardd::proc::stop_event`, where launchd sends
//!   SIGTERM), ends it if it hasn't within [`GRACE`], and exits; [`restart_daemon`] then starts a new
//!   one, on the programs now in the install.
//! - **Links**: `<scheme>://…` runs `taskboard-app.exe "<url>"`. One app runs per install
//!   ([`claim`]): a second launch hands its links, or just "come to the front", to the first over a
//!   named pipe and exits. (Not an AF_UNIX socket: its file outlives an app that crashes, and
//!   nothing can delete it or listen there again.)
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenMutexW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, SYNCHRONIZATION_SYNCHRONIZE,
    TerminateProcess, WaitForSingleObject,
};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

/// The install's description, beside its programs.
pub const MANIFEST: &str = "install.json";
pub const APP_EXE: &str = "taskboard-app.exe";
pub const DAEMON_EXE: &str = "taskboardd.exe";
/// The supervisor waits this long between starts of a daemon that keeps crashing (launchd's ThrottleInterval).
const THROTTLE: Duration = Duration::from_secs(10);
/// How long a daemon asked to stop has before it's ended: inside [`stop`]'s 10 seconds.
const GRACE: Duration = Duration::from_secs(8);
/// How long [`stop`] waits for the supervisor, and for the daemon in all.
const STOP_LIMIT: Duration = Duration::from_secs(10);
/// A message to the running app ([`claim`]) that quits it instead of opening a link.
pub const QUIT: &str = "--quit";

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Where Settings ▸ Apps ▸ Startup (and Task Manager) keep a Run entry switched off.
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// What `install.json` says about an install. A missing file or field is the real Taskboard's.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    /// `com.mrgnhnt.taskboard`: the updater only replaces an install with this id.
    pub id: String,
    /// What Windows shows: the Start menu, Startup apps.
    pub name: String,
    /// Links this install opens: `taskboard` for `taskboard://task/T12`.
    pub scheme: String,
    /// The daemon's label (the LaunchAgent's on macOS): names its supervisor.
    pub label: String,
    pub version: Option<String>,
    /// Set for the app, and so for the supervisor and the daemon it starts.
    pub env: Vec<(String, String)>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest::parse(&Value::Null)
    }
}

impl Manifest {
    pub fn parse(v: &Value) -> Manifest {
        let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        let id = s("id").unwrap_or_else(|| "com.mrgnhnt.taskboard".into());
        Manifest {
            name: s("name").unwrap_or_else(|| "Taskboard".into()),
            scheme: s("scheme").unwrap_or_else(|| "taskboard".into()),
            label: s("label").unwrap_or_else(|| format!("{id}.daemon")),
            version: s("version"),
            env: v["env"].as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect(),
            id,
        }
    }

    pub fn read(dir: &Path) -> Manifest {
        let v = std::fs::read(dir.join(MANIFEST)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        Manifest::parse(&v)
    }
}

/// The install this program runs from and its manifest; None for a dev build.
pub fn install() -> Option<(PathBuf, Manifest)> {
    let dir = crate::install::installed_app()?;
    let m = Manifest::read(&dir);
    Some((dir, m))
}

/// Takes on the install's environment (`install.json`'s `env`), before anything reads it: the log,
/// the config, the daemon the supervisor starts.
pub fn apply_env() {
    let Some((_, m)) = install() else { return };
    for (k, v) in m.env {
        // SAFETY: first thing in main, before any other thread starts.
        unsafe { std::env::set_var(k, v) };
    }
}

/// `taskboard-app.exe --version` and the like print to the terminal they're run from, though the
/// app has no console of its own.
pub fn attach_console() {
    unsafe { windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS) };
}

/// Where this install keeps its downloaded updates.
pub fn local_dir(m: &Manifest) -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| taskboardd::util::expand_home("~/AppData/Local"));
    local.join(&m.id)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ------------------------------------------------------------------ named mutexes

/// A named mutex held for the life of this process: Windows drops it when the process exits.
pub struct Held(HANDLE);

// SAFETY: a handle is just a number to Windows; nothing else here touches it.
unsafe impl Send for Held {}

impl Drop for Held {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Makes the mutex, unless another process has it already.
fn hold(name: &str) -> Option<Held> {
    let h = unsafe { CreateMutexW(std::ptr::null(), 0, wide(name).as_ptr()) };
    if h.is_null() {
        return None;
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(h) };
        return None;
    }
    Some(Held(h))
}

fn held(name: &str) -> bool {
    let h = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide(name).as_ptr()) };
    if h.is_null() {
        return false;
    }
    unsafe { CloseHandle(h) };
    true
}

/// Waits up to `limit` for the mutex to go (its process to exit).
fn wait_released(name: &str, limit: Duration) -> bool {
    let start = Instant::now();
    while held(name) {
        if start.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

fn supervisor_lock(m: &Manifest) -> String {
    format!(r"Local\{}", m.label)
}

fn app_lock(m: &Manifest) -> String {
    format!(r"Local\{}.app", m.id)
}

// ------------------------------------------------------------------ processes

/// The processes whose program is `name` inside `dir` (or a copy an update moved aside there).
pub fn running_under(dir: &Path, name: &str) -> Vec<u32> {
    let dir = dir.to_string_lossy().to_string();
    let mut found = Vec::new();
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return found;
    }
    let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut ok = unsafe { Process32FirstW(snap, &mut e) } != 0;
    while ok {
        let exe = String::from_utf16_lossy(&e.szExeFile[..e.szExeFile.iter().position(|&c| c == 0).unwrap_or(260)]);
        if exe.eq_ignore_ascii_case(name)
            && let Some(path) = image_path(e.th32ProcessID)
            && taskboardd::util::is_under(&path, &dir)
        {
            found.push(e.th32ProcessID);
        }
        ok = unsafe { Process32NextW(snap, &mut e) } != 0;
    }
    unsafe { CloseHandle(snap) };
    found
}

fn image_path(pid: u32) -> Option<String> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } != 0;
    unsafe { CloseHandle(h) };
    ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// Ends the process with `code` and waits (up to 5s) for it to be gone.
fn end(pid: u32, code: u32) -> bool {
    let h = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return false;
    }
    let ended = unsafe { TerminateProcess(h, code) } != 0 && unsafe { WaitForSingleObject(h, 5000) } == WAIT_OBJECT_0;
    unsafe { CloseHandle(h) };
    ended
}

/// Waits up to `limit` for the process to exit (true: it has, or there was none).
pub fn wait_exit(pid: u32, limit: Duration) -> bool {
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return true;
    }
    let gone = unsafe { WaitForSingleObject(h, ms(limit)) } == WAIT_OBJECT_0;
    unsafe { CloseHandle(h) };
    gone
}

/// A wait's milliseconds.
fn ms(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

/// Makes a manual-reset named event, or opens it if it's there already.
fn event(name: &str) -> Option<Held> {
    let h = unsafe { windows_sys::Win32::System::Threading::CreateEventW(std::ptr::null(), 1, 0, wide(name).as_ptr()) };
    (!h.is_null()).then_some(Held(h))
}

/// Asks `taskboardd serve` (`pid`, with its `process` handle) to stop cleanly: sets its stop event,
/// made here if it hasn't got that far in starting up, so it stops as soon as it does. True when it
/// exited within `limit`.
fn ask_to_stop(pid: u32, process: HANDLE, limit: Duration) -> bool {
    let Some(ev) = event(&taskboardd::proc::stop_event(pid)) else { return false };
    unsafe { windows_sys::Win32::System::Threading::SetEvent(ev.0) };
    let exited = unsafe { WaitForSingleObject(process, ms(limit)) } == WAIT_OBJECT_0;
    exited
}

/// Asks the daemon `pid` to stop and ends it if it hasn't within `limit`. True when it stopped by itself.
fn stop_daemon(pid: u32, limit: Duration) -> bool {
    // One handle throughout, so the id can't go to another process meanwhile.
    let h = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return false;
    }
    let asked = ask_to_stop(pid, h, limit);
    if !asked {
        unsafe { TerminateProcess(h, 0) };
        unsafe { WaitForSingleObject(h, 5000) };
    }
    unsafe { CloseHandle(h) };
    asked
}

/// Starts a program with no console and outside the job of whatever started us (a terminal that
/// ends its whole tree when it closes), so it outlives this process.
pub fn spawn_detached(cmd: &mut Command) -> std::io::Result<()> {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    // A job that doesn't allow breaking away refuses the flag; then start inside it.
    cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn().or_else(|_| cmd.creation_flags(flags).spawn()).map(|_| ())
}

// ------------------------------------------------------------------ the daemon

/// `taskboard-app.exe --daemon`: keeps `taskboardd.exe serve` running (see the module docs). One
/// per install; another just exits.
pub fn supervise() -> i32 {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else { return 2 };
    let m = Manifest::read(&dir);
    let Some(_lock) = hold(&supervisor_lock(&m)) else { return 0 };
    // [`stop`] sets it: stop the daemon and go, even while waiting to start it again.
    let Some(stop) = event(&stop_event(&m)) else { return 2 };
    // Kept from the start: an update moves this program aside, and the daemon to start is the one now in the install.
    let daemon = dir.join(DAEMON_EXE);
    keep_running(
        || {
            let mut c = Command::new(&daemon);
            c.arg("serve");
            c
        },
        &stop,
        THROTTLE,
        GRACE,
    )
}

/// Runs the command `daemon` makes, with no window, and starts it again after it fails, at most
/// every `throttle`, until it exits cleanly or `stop` is set. Then it asks the daemon to stop and
/// ends it if it hasn't within `grace`.
fn keep_running(daemon: impl Fn() -> Command, stop: &Held, throttle: Duration, grace: Duration) -> i32 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::WaitForMultipleObjects;
    let stopped = |wait: Duration| unsafe { WaitForSingleObject(stop.0, ms(wait)) } == WAIT_OBJECT_0;
    let mut failed_starts = 0;
    loop {
        let started = Instant::now();
        let mut cmd = daemon();
        let child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).spawn();
        match child {
            Err(e) => {
                failed_starts += 1;
                crate::log::info(format!("couldn't start {}: {e}", Path::new(cmd.get_program()).display()));
                // Gone for a minute: uninstalled, not mid-update.
                if failed_starts >= 6 {
                    return 1;
                }
            }
            Ok(mut c) => {
                failed_starts = 0;
                let handles = [c.as_raw_handle() as HANDLE, stop.0];
                if unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX) } == WAIT_OBJECT_0 + 1 {
                    if ask_to_stop(c.id(), handles[0], grace) {
                        crate::log::info("stopped the daemon");
                    } else {
                        crate::log::info(format!("the daemon didn't stop within {}s of being asked; ending it", grace.as_secs_f32()));
                        unsafe { TerminateProcess(handles[0], 0) };
                    }
                    let _ = c.wait();
                    return 0;
                }
                match c.wait() {
                    Ok(s) if s.success() => {
                        crate::log::info("the daemon exited cleanly; not starting it again");
                        return 0;
                    }
                    Ok(s) => crate::log::info(format!("the daemon exited ({s}); starting it again")),
                    Err(e) => crate::log::info(format!("lost track of the daemon: {e}")),
                }
            }
        }
        if stopped(throttle.saturating_sub(started.elapsed())) {
            return 0;
        }
    }
}

fn stop_event(m: &Manifest) -> String {
    format!(r"Local\{}.stop", m.label)
}

pub fn supervisor_running(m: &Manifest) -> bool {
    held(&supervisor_lock(m))
}

fn start_supervisor(dir: &Path) -> Result<(), String> {
    spawn_detached(Command::new(dir.join(APP_EXE)).arg("--daemon")).map_err(|e| format!("couldn't start the board's background service: {e}"))
}

fn run_command(dir: &Path) -> String {
    format!("\"{}\" --daemon", dir.join(APP_EXE).display())
}

/// Switched off in Settings ▸ Apps ▸ Startup (or Task Manager): an odd first byte.
fn startup_disabled(name: &str) -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey(APPROVED).and_then(|k| k.get_raw_value(name)).is_ok_and(|v| v.bytes.first().is_some_and(|b| b & 1 == 1))
}

/// Sign-in starts the daemon's supervisor (the Run entry), the link scheme opens this app, the
/// Start menu and Settings ▸ Apps list it, and the daemon runs now if it doesn't already. Run at every launch: an install that moved, or an
/// update, points them at itself again.
pub fn register(dir: &Path, m: &Manifest) -> crate::install::LoginItem {
    use crate::install::LoginItem;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run = hkcu.create_subkey(RUN).and_then(|(k, _)| {
        let want = run_command(dir);
        if k.get_value::<String, _>(&m.name).ok().as_deref() != Some(want.as_str()) { k.set_value(&m.name, &want) } else { Ok(()) }
    });
    if let Err(e) = run {
        return LoginItem::Failed(format!("Windows wouldn't add it to the startup apps: {e}"));
    }
    if let Err(e) = register_scheme(dir, m) {
        crate::log::info(format!("couldn't register {}:// links: {e}", m.scheme));
    }
    if let Err(e) = crate::win_install::add_to_start_menu(dir, m) {
        crate::log::info(format!("couldn't add it to the Start menu: {e}"));
    }
    if let Err(e) = crate::win_install::add_to_apps(dir, m) {
        crate::log::info(format!("couldn't add it to Settings ▸ Apps: {e}"));
    }
    if !supervisor_running(m)
        && let Err(e) = start_supervisor(dir)
    {
        return LoginItem::Failed(e);
    }
    // Switched off, it still runs now (the app needs it), but not at the next sign-in.
    startup_status(m)
}

/// Whether the startup entry is switched on in Settings ▸ Apps ▸ Startup, as it is now; it changes
/// nothing (`install::recheck`).
pub fn startup_status(m: &Manifest) -> crate::install::LoginItem {
    use crate::install::LoginItem;
    if startup_disabled(&m.name) { LoginItem::RequiresApproval } else { LoginItem::Enabled }
}

/// The daemon again on the binary now in the install. Its supervisor is replaced too, so after an
/// update nothing runs from the old files.
pub fn restart_daemon(dir: &Path, m: &Manifest) -> Result<(), String> {
    if !stop(dir, m) {
        return Err("the daemon's supervisor didn't stop".into());
    }
    start_supervisor(dir)
}

/// Stops the daemon and its supervisor, and a daemon from the install that runs without one: each
/// is asked first and ended if it hasn't stopped within [`STOP_LIMIT`] in all.
pub fn stop(dir: &Path, m: &Manifest) -> bool {
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
    let start = Instant::now();
    let ev = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, wide(&stop_event(m)).as_ptr()) };
    if !ev.is_null() {
        unsafe { SetEvent(ev) };
        unsafe { CloseHandle(ev) };
    }
    let gone = wait_released(&supervisor_lock(m), STOP_LIMIT);
    // Run by hand from the install, or left by a supervisor that crashed: what's left of the time.
    for pid in running_under(dir, DAEMON_EXE) {
        stop_daemon(pid, STOP_LIMIT.saturating_sub(start.elapsed()));
    }
    gone
}

/// Takes the Run entry and the link scheme out and stops the daemon.
pub fn unregister(dir: &Path, m: &Manifest) -> Result<(), String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut err = None;
    if let Ok(k) = hkcu.open_subkey_with_flags(RUN, winreg::enums::KEY_ALL_ACCESS)
        && let Err(e) = k.delete_value(&m.name)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        err = Some(format!("couldn't take it out of the startup apps: {e}"));
    }
    if let Ok(k) = hkcu.open_subkey_with_flags(APPROVED, winreg::enums::KEY_ALL_ACCESS) {
        let _ = k.delete_value(&m.name);
    }
    if scheme_is_ours(dir, m) {
        let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{}", m.scheme));
    }
    if !stop(dir, m) {
        err.get_or_insert_with(|| "the daemon's supervisor didn't stop".into());
    }
    err.map_or(Ok(()), Err)
}

/// Settings ▸ Apps ▸ Startup, where a switched-off startup app is switched on.
pub fn open_startup_apps() {
    let _ = Command::new("explorer").arg("ms-settings:startupapps").spawn();
}

// ------------------------------------------------------------------ links

fn open_command(dir: &Path) -> String {
    format!("\"{}\" \"%1\"", dir.join(APP_EXE).display())
}

fn register_scheme(dir: &Path, m: &Manifest) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(format!(r"Software\Classes\{}", m.scheme))?;
    if key.open_subkey(r"shell\open\command").and_then(|k| k.get_value::<String, _>("")).ok() == Some(open_command(dir)) {
        return Ok(());
    }
    key.set_value("", &format!("URL:{} link", m.name))?;
    key.set_value("URL Protocol", &"")?;
    key.create_subkey("DefaultIcon")?.0.set_value("", &format!("\"{}\",0", dir.join(APP_EXE).display()))?;
    key.create_subkey(r"shell\open\command")?.0.set_value("", &open_command(dir))
}

fn scheme_is_ours(dir: &Path, m: &Manifest) -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let cmd = hkcu.open_subkey(format!(r"Software\Classes\{}\shell\open\command", m.scheme)).and_then(|k| k.get_value::<String, _>(""));
    cmd.is_ok_and(|c| taskboardd::util::path_key(&c).contains(&taskboardd::util::path_key(&dir.join(APP_EXE).to_string_lossy())))
}

/// The links among a launch's arguments: what Windows passes for a clicked `<scheme>://` link.
pub fn links(args: &[String]) -> Vec<String> {
    args.iter().skip(1).filter(|a| a.contains("://")).cloned().collect()
}

/// The install's pipe: one per user, as the board runs per user.
fn pipe_name(m: &Manifest) -> String {
    let user: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    format!(r"\\.\pipe\{}.app-{user}", m.id)
}

/// The server end of the install's pipe, waiting for a launch to connect.
pub struct Pipe {
    name: String,
    next: HANDLE,
}

// SAFETY: a handle is just a number to Windows; only the links thread uses it.
unsafe impl Send for Pipe {}

/// A new instance of the pipe for the next launch to connect to. The first refuses to share the
/// name with a pipe someone else made.
fn pipe_instance(name: &str, first: bool) -> Option<HANDLE> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
    let open = PIPE_ACCESS_INBOUND | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let h = unsafe { CreateNamedPipeW(wide(name).as_ptr(), open, mode, PIPE_UNLIMITED_INSTANCES, 0, 64 * 1024, 0, std::ptr::null()) };
    (h != INVALID_HANDLE_VALUE).then_some(h)
}

/// What [`claim`] decided.
pub enum Claim {
    /// This is the install's one app: what other launches send arrives at the pipe, one message (its lines) at a time.
    First(Held, Pipe),
    /// Another app had it, and took the messages.
    HandedOver,
    /// Another app has it but didn't answer; run anyway.
    Alone,
}

/// Become this install's one app, or hand `msgs` (links, or none: come to the front) to the one
/// running.
pub fn claim(m: &Manifest, msgs: &[String]) -> Claim {
    claim_named(&app_lock(m), &pipe_name(m), msgs)
}

fn claim_named(lock: &str, name: &str, msgs: &[String]) -> Claim {
    if let Some(lock) = hold(lock) {
        return match pipe_instance(name, true) {
            Some(h) => Claim::First(lock, Pipe { name: name.to_string(), next: h }),
            None => {
                crate::log::info(format!("couldn't make the pipe {name}: {}", std::io::Error::last_os_error()));
                Claim::Alone
            }
        };
    }
    // The first one may still be starting up.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if send(name, msgs) {
            return Claim::HandedOver;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Claim::Alone
}

fn send(name: &str, msgs: &[String]) -> bool {
    let Ok(mut pipe) = std::fs::OpenOptions::new().write(true).open(name) else { return false };
    // Lets the app take the foreground: Windows only allows it to whoever the human just used.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY) };
    pipe.write_all(msgs.join("\n").as_bytes()).is_ok()
}

/// Passes each message that arrives at the pipe on to `tx`, on a thread of its own.
pub fn listen(pipe: Pipe, tx: async_channel::Sender<Vec<String>>) {
    use windows_sys::Win32::Foundation::{ERROR_NO_DATA, ERROR_PIPE_CONNECTED};
    use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
    // The handle crosses to the thread as a number (a raw pointer isn't Send).
    let (name, first) = (pipe.name, pipe.next as usize);
    let _ = std::thread::Builder::new().name("links".into()).spawn(move || {
        let pipe = Pipe { name, next: first as HANDLE };
        let mut current = pipe.next;
        loop {
            // A launch can connect before this waits for it (PIPE_CONNECTED), or even write and
            // disconnect (NO_DATA): its message is still in the pipe either way.
            let connected = unsafe { ConnectNamedPipe(current, std::ptr::null_mut()) } != 0 || matches!(unsafe { GetLastError() }, ERROR_PIPE_CONNECTED | ERROR_NO_DATA);
            // The next launch's instance, before this one's message is read.
            let Some(next) = pipe_instance(&pipe.name, false) else {
                crate::log::info(format!("the pipe {} stopped: {}", pipe.name, std::io::Error::last_os_error()));
                return;
            };
            // SAFETY: `current` is a pipe instance this thread made and nothing else holds.
            let mut conn = unsafe { std::fs::File::from_raw_handle(current as _) };
            current = next;
            let mut body = String::new();
            // A launch that ends without writing just breaks the pipe: an empty message.
            if !connected || conn.read_to_string(&mut body).is_err() {
                continue;
            }
            let msgs: Vec<String> = body.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
            if tx.send_blocking(msgs).is_err() {
                return;
            }
        }
    });
}

/// Everything that runs from the install stops, so a release folder can replace it or an uninstall
/// delete it: the daemon and its supervisor, then the app, asked to quit. An app that doesn't within
/// 10 seconds is ended. True when nothing is left running (an agent's `tb.exe` aside).
pub fn shut_down(dir: &Path, m: &Manifest) -> bool {
    // The app first: it starts the daemon again if it finds it gone.
    let quit = !held(&app_lock(m)) || (send(&pipe_name(m), &[QUIT.to_string()]) && wait_released(&app_lock(m), Duration::from_secs(10)));
    let mut stopped = stop(dir, m);
    // An app that didn't quit: with the supervisor gone, any other taskboard-app.exe from the install is one.
    let me = std::process::id();
    let left: Vec<u32> = running_under(dir, APP_EXE).into_iter().filter(|&p| p != me).collect();
    if !quit || !left.is_empty() {
        crate::log::info("the app didn't quit; ending it");
        for pid in left {
            end(pid, 1);
        }
        // It may have started the daemon again meanwhile.
        stopped = stop(dir, m);
    }
    stopped && running_under(dir, APP_EXE).iter().all(|&p| p == me) && running_under(dir, DAEMON_EXE).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_manifest_names_the_install_and_its_environment() {
        let m = Manifest::parse(&json!({"id": "com.mrgnhnt.taskboard.dev", "name": "Taskboard Dev", "scheme": "taskboard-dev", "version": "1.2.0",
            "env": {"TASKBOARD_PORT": "18792", "skip": 3}}));
        assert_eq!((m.id.as_str(), m.name.as_str(), m.scheme.as_str()), ("com.mrgnhnt.taskboard.dev", "Taskboard Dev", "taskboard-dev"));
        assert_eq!(m.label, "com.mrgnhnt.taskboard.dev.daemon", "the label follows the id");
        assert_eq!(m.version.as_deref(), Some("1.2.0"));
        assert_eq!(m.env, vec![("TASKBOARD_PORT".to_string(), "18792".to_string())]);
        let none = Manifest::default();
        assert_eq!((none.id.as_str(), none.name.as_str(), none.scheme.as_str(), none.label.as_str()), ("com.mrgnhnt.taskboard", "Taskboard", "taskboard", "com.mrgnhnt.taskboard.daemon"));
    }

    #[test]
    fn a_launch_s_links_are_its_url_arguments() {
        let args = ["app.exe", "--after", "12", "taskboard://task/T3", "taskboard-dev://goal/G1"].map(String::from);
        assert_eq!(links(&args), vec!["taskboard://task/T3", "taskboard-dev://goal/G1"]);
        assert!(links(&["app.exe".to_string()]).is_empty());
    }

    /// The next message the links thread passes on, or a failure after 10 seconds rather than a
    /// test that never ends.
    fn next(rx: &async_channel::Receiver<Vec<String>>) -> Vec<String> {
        let start = Instant::now();
        loop {
            match rx.try_recv() {
                Ok(msgs) => return msgs,
                Err(async_channel::TryRecvError::Closed) => panic!("the links thread stopped"),
                Err(_) if start.elapsed() > Duration::from_secs(10) => panic!("no message within 10 seconds"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn one_app_per_install_and_a_second_launch_hands_its_links_over() {
        let id = format!("taskboard-test-claim-{}", std::process::id());
        let (lock, name) = (format!(r"Local\{id}.app"), format!(r"\\.\pipe\{id}.app"));
        let Claim::First(held, pipe) = claim_named(&lock, &name, &[]) else { panic!("the first launch is the app") };
        let (tx, rx) = async_channel::unbounded();
        listen(pipe, tx);
        assert!(matches!(claim_named(&lock, &name, &["taskboard://task/T7".into()]), Claim::HandedOver));
        assert_eq!(next(&rx), vec!["taskboard://task/T7"]);
        assert!(matches!(claim_named(&lock, &name, &[]), Claim::HandedOver), "no links: just come to the front");
        assert!(next(&rx).is_empty());
        assert!(matches!(claim_named(&lock, &name, &["a".into(), "b".into()]), Claim::HandedOver));
        assert_eq!(next(&rx), vec!["a", "b"], "one message per launch, its lines");
        drop(held);
    }

    #[test]
    fn a_launch_that_s_come_and_gone_before_the_app_gets_to_it_still_hands_its_links_over() {
        let id = format!("taskboard-test-early-{}", std::process::id());
        let (lock, name) = (format!(r"Local\{id}.app"), format!(r"\\.\pipe\{id}.app"));
        let Claim::First(held, pipe) = claim_named(&lock, &name, &[]) else { panic!("the first launch is the app") };
        // Connects, writes and disconnects before the app waits for a connection.
        assert!(send(&name, &["taskboard://task/T9".into()]));
        let (tx, rx) = async_channel::unbounded();
        listen(pipe, tx);
        assert_eq!(next(&rx), vec!["taskboard://task/T9"]);
        drop(held);
    }

    #[test]
    fn a_pipe_someone_else_made_isn_t_taken_over() {
        let name = format!(r"\\.\pipe\taskboard-test-squat-{}", std::process::id());
        let theirs = pipe_instance(&name, true).unwrap();
        assert!(pipe_instance(&name, true).is_none(), "the first instance must be ours");
        unsafe { CloseHandle(theirs) };
    }

    #[test]
    fn a_mutex_is_held_until_its_holder_lets_go() {
        let name = format!(r"Local\taskboard-test-hold-{}", std::process::id());
        assert!(!held(&name));
        let lock = hold(&name).unwrap();
        assert!(held(&name));
        assert!(hold(&name).is_none(), "only one holder");
        drop(lock);
        assert!(wait_released(&name, Duration::from_secs(1)));
    }

    #[test]
    fn finds_and_ends_a_program_running_from_a_folder() {
        let dir = std::env::temp_dir().join(format!("taskboard-running-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ping = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\PING.EXE");
        std::fs::copy(&ping, dir.join("PING.EXE")).unwrap();
        let mut child = Command::new(dir.join("PING.EXE")).args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
        let pids = running_under(&dir, "ping.exe");
        assert_eq!(pids, vec![child.id()], "only the copy in this folder, not other pings");
        assert!(end(child.id(), 7));
        assert_eq!(child.wait().unwrap().code(), Some(7), "ended with the code it was given");
        assert!(running_under(&dir, "ping.exe").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Not a test: the daemon the supervisor tests run, which is this test program again with
    /// `TASKBOARD_FAKE_DAEMON` naming a file it writes what it does to. By its
    /// `TASKBOARD_FAKE_DAEMON_MODE`: `listen` waits to be asked to stop the way `taskboardd serve`
    /// does and exits 0 when it is; `late` starts listening only after 1.5 seconds; `deaf` never
    /// listens; `crash` exits 3 at once, and `done` 0.
    #[test]
    #[ignore]
    fn fake_daemon() {
        let Some(out) = std::env::var_os("TASKBOARD_FAKE_DAEMON") else { return };
        let say = |s: &str| {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).unwrap();
            writeln!(f, "{s}").unwrap();
        };
        match std::env::var("TASKBOARD_FAKE_DAEMON_MODE").unwrap_or_default().as_str() {
            "crash" => {
                say("started");
                std::process::exit(3);
            }
            "done" => {
                say("started");
                std::process::exit(0);
            }
            "deaf" => {
                say("deaf");
                std::thread::sleep(Duration::from_secs(60));
                std::process::exit(4);
            }
            "late" => std::thread::sleep(Duration::from_millis(1500)),
            _ => {}
        }
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(taskboardd::proc::on_stop_asked(move || tx.send(()).unwrap()));
        say("listening");
        if rx.recv_timeout(Duration::from_secs(60)).is_err() {
            std::process::exit(5);
        }
        say("stopped cleanly");
        std::process::exit(0);
    }

    /// Makes the command that starts [`fake_daemon`] in `mode`, reporting to `out`.
    fn fake(out: &Path, mode: &str) -> impl Fn() -> Command + Send + 'static {
        let (exe, out, mode) = (std::env::current_exe().unwrap(), out.to_path_buf(), mode.to_string());
        move || {
            let mut c = Command::new(&exe);
            c.args(["win::tests::fake_daemon", "--exact", "--ignored", "--test-threads=1", "-q"]);
            c.env("TASKBOARD_FAKE_DAEMON", &out).env("TASKBOARD_FAKE_DAEMON_MODE", &mode);
            c
        }
    }

    /// A file for one test's fake daemon to report to, in a folder of the test's own.
    fn report(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("taskboard-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("daemon.txt")
    }

    fn lines(out: &Path) -> Vec<String> {
        std::fs::read_to_string(out).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Waits (up to 30 seconds, the whole suite may be running) until the fake has said `what` `n` times.
    fn wait_for(out: &Path, what: &str, n: usize) {
        let start = Instant::now();
        while lines(out).iter().filter(|l| *l == what).count() < n {
            assert!(start.elapsed() < Duration::from_secs(30), "the fake daemon didn't say {what:?} {n} times: {:?}", lines(out));
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The supervisor's exit code, or a failure if it doesn't finish within `limit`.
    fn finished(sup: std::thread::JoinHandle<i32>, limit: Duration) -> i32 {
        let start = Instant::now();
        while !sup.is_finished() {
            assert!(start.elapsed() < limit, "the supervisor didn't finish within {limit:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        sup.join().unwrap()
    }

    /// The supervisor on a thread, with a stop event of the test's own; the other handle sets it.
    fn supervisor(test: &str, out: &Path, mode: &str, throttle: Duration, grace: Duration) -> (std::thread::JoinHandle<i32>, Held) {
        let name = format!(r"Local\taskboard-test-{test}-{}", std::process::id());
        let (ours, theirs) = (event(&name).unwrap(), event(&name).unwrap());
        let daemon = fake(out, mode);
        (std::thread::spawn(move || keep_running(daemon, &theirs, throttle, grace)), ours)
    }

    fn set(ev: &Held) {
        unsafe { windows_sys::Win32::System::Threading::SetEvent(ev.0) };
    }

    #[test]
    fn a_stopped_supervisor_asks_its_daemon_to_stop_and_it_stops_cleanly() {
        let out = report("clean-stop");
        let (sup, stop) = supervisor("clean-stop", &out, "listen", Duration::from_secs(10), Duration::from_secs(10));
        wait_for(&out, "listening", 1);
        let asked = Instant::now();
        set(&stop);
        assert_eq!(finished(sup, Duration::from_secs(15)), 0);
        assert!(asked.elapsed() < Duration::from_secs(10), "it stopped when asked, not when its time ran out");
        assert_eq!(lines(&out), ["listening", "stopped cleanly"]);
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[test]
    fn a_daemon_asked_to_stop_while_it_starts_stops_once_it_listens() {
        let out = report("late-stop");
        let mut c = fake(&out, "late")().stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        assert!(stop_daemon(c.id(), Duration::from_secs(15)), "it stopped by itself");
        assert_eq!(c.wait().unwrap().code(), Some(0));
        assert_eq!(lines(&out), ["listening", "stopped cleanly"]);
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[test]
    fn a_daemon_that_doesn_t_stop_when_asked_is_ended() {
        let out = report("deaf-stop");
        let (sup, stop) = supervisor("deaf-stop", &out, "deaf", Duration::from_secs(10), Duration::from_millis(500));
        wait_for(&out, "deaf", 1);
        let asked = Instant::now();
        set(&stop);
        assert_eq!(finished(sup, Duration::from_secs(10)), 0);
        assert!(asked.elapsed() >= Duration::from_millis(500), "it had its time to stop first");
        assert_eq!(lines(&out), ["deaf"]);
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }

    #[test]
    fn a_daemon_that_crashes_is_started_again_but_not_one_that_exits_cleanly() {
        let out = report("crash");
        let (sup, stop) = supervisor("crash", &out, "crash", Duration::from_millis(100), Duration::from_secs(1));
        wait_for(&out, "started", 3);
        set(&stop);
        assert_eq!(finished(sup, Duration::from_secs(10)), 0);
        let out = report("done");
        let (sup, _stop) = supervisor("done", &out, "done", Duration::from_millis(100), Duration::from_secs(1));
        assert_eq!(finished(sup, Duration::from_secs(30)), 0, "a clean exit ends the supervisor too");
        assert_eq!(lines(&out), ["started"], "and isn't started again");
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("taskboard-crash-{}", std::process::id())));
    }
}
