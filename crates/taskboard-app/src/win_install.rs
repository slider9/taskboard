//! Windows: the release folder installs itself, as Midna's does; there's no separate installer.
//!
//! - **The release folder** (`packaging/windows/build.sh`, the `Taskboard` folder in the release
//!   zip) holds the install's files and [`MARKER`]. Run from anywhere but its install folder
//!   (`%LOCALAPPDATA%\Programs\<name>`, [`programs_dir`]), `taskboard-app.exe` doesn't run there
//!   ([`place`], before anything else at launch):
//!   - nothing installed: it copies the folder in and starts that copy;
//!   - the same version installed: it starts that copy (a running one comes to the front);
//!   - an older one: it quits the app and stops the daemon, swaps the files in as an update does
//!     (`updater::swap_in`) and starts the new copy;
//!   - a newer one: it refuses, saying to uninstall that one first.
//!
//!   A folder holding anything but the release's files ([`not_the_build`]) isn't installed from,
//!   so a marker among someone's files never copies them. The copies lose the Mark of the Web, so
//!   the installed app isn't checked by SmartScreen again after the person ran that very build.
//! - **Each launch from the install** (`win::register`) adds it to the Start menu
//!   ([`add_to_start_menu`]) and to Settings ▸ Apps ([`add_to_apps`], whose Uninstall runs
//!   `taskboard-app.exe --uninstall`), and `install::register_and_link` puts `~\.local\bin`, where
//!   `tb.exe` is copied, on the user's `Path` ([`put_on_path`]).
//! - **Uninstall** takes those out again (the `Path` entry only if the install added it), then a copy
//!   of the app in `%TEMP%` deletes the install's files once the uninstall has exited
//!   ([`remove_later`], [`remove_install`]). The board's data stays.
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_ALL_ACCESS, REG_EXPAND_SZ, REG_SZ};

use crate::win::{APP_EXE, DAEMON_EXE, MANIFEST, Manifest};

/// A release folder has this file beside `taskboard-app.exe`; without it the app never installs
/// itself from a folder, or counts one as an install.
pub const MARKER: &str = "taskboard-bundle.txt";
/// Everything a release folder holds at its top (`packaging/windows/build.sh`). A file the
/// release gains must be added here.
const RELEASE_FILES: [&str; 5] = [APP_EXE, DAEMON_EXE, "tb.exe", MANIFEST, MARKER];
const RELEASE_DIRS: [&str; 2] = ["Fonts", "plugin"];
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `%LOCALAPPDATA%\Programs\<name>`: where the install `m` names lives.
pub fn programs_dir(m: &Manifest) -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| taskboardd::util::expand_home("~/AppData/Local"));
    local.join("Programs").join(&m.name)
}

/// The same folder, however it's spelled (case, `/`, a trailing separator, `\\?\`).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    let full = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_string();
    taskboardd::util::path_key(&full(a)) == taskboardd::util::path_key(&full(b))
}

/// What a launch from a release folder does ([`place`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Placement {
    /// This is the install folder: run as usual.
    Here,
    /// Nothing is installed: copy this in, then start that copy.
    Install,
    /// This version is installed: start that copy.
    StartInstalled,
    /// An older version is installed (or one whose version can't be read): replace it, then start it.
    Update,
    /// A newer version is installed: refuse.
    Newer(String),
    /// Another app is installed there (its id): refuse.
    Other(String),
}

/// What to do for a release folder `here` (its manifest `ours`) whose install folder is `dest`,
/// where `installed` is the manifest of what's installed there (None: nothing is).
pub fn placement(here: &Path, dest: &Path, ours: &Manifest, installed: Option<&Manifest>) -> Placement {
    if same_dir(here, dest) {
        return Placement::Here;
    }
    let Some(theirs) = installed else { return Placement::Install };
    if theirs.id != ours.id {
        return Placement::Other(theirs.id.clone());
    }
    let (have, want) = (theirs.version.as_deref().unwrap_or(""), ours.version.as_deref().unwrap_or(crate::updater::VERSION));
    match () {
        _ if have == want => Placement::StartInstalled,
        _ if crate::updater::is_newer(have, want) => Placement::Newer(have.to_string()),
        _ => Placement::Update,
    }
}

/// The names in `dir` (a release folder to install from) that aren't the release's: anything but
/// [`RELEASE_FILES`] and the [`RELEASE_DIRS`] folders at its top. Its whole folder is copied, so
/// a marker among someone's files must never copy those.
pub fn not_the_build(dir: &Path) -> Result<Vec<String>, String> {
    let mut others = vec![];
    for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        let ours = if is_dir { RELEASE_DIRS.iter().any(|d| d.eq_ignore_ascii_case(&name)) } else { RELEASE_FILES.iter().any(|f| f.eq_ignore_ascii_case(&name)) };
        if !ours {
            others.push(name);
        }
    }
    others.sort();
    Ok(others)
}

/// Copies the folder `from` to `to` (which mustn't exist), each file without its Mark of the Web.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
            drop_web_mark(&dst);
        }
    }
    Ok(())
}

/// Takes the Mark of the Web (the `Zone.Identifier` stream) off a copied file. Explorer's Extract
/// All marks every file of a downloaded zip and a copy keeps it, so the installed app would go
/// through SmartScreen again from the Start menu after the person already ran this very build.
fn drop_web_mark(file: &Path) {
    let mut stream = file.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    let _ = std::fs::remove_file(PathBuf::from(stream));
}

/// Copies the release folder `from` into `dest` (made if need be), replacing what's there the way
/// an update does: staged inside `dest` first, then each entry moved into place, all undone if one
/// fails (`updater::swap_in`).
pub fn copy_in(from: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let stage = dest.join(format!(".install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    let staged = stage.join("Taskboard");
    if let Err(e) = copy_tree(from, &staged) {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(format!("couldn't copy it into {}: {e}", dest.display()));
    }
    // The marker last, so a folder with it holds the whole install. Kept outside the stage, which
    // the swap deletes when it's done.
    let held = dest.join(format!(".install-{}-{MARKER}", std::process::id()));
    let r = std::fs::rename(staged.join(MARKER), &held)
        .map_err(|e| format!("{}: {e}", held.display()))
        .and_then(|()| crate::updater::swap_in(&staged, dest))
        .and_then(|()| std::fs::rename(&held, dest.join(MARKER)).map_err(|e| format!("{}: {e}", dest.join(MARKER).display())));
    let _ = std::fs::remove_dir_all(&stage);
    let _ = std::fs::remove_file(&held);
    r
}

/// Launched from a release folder (one with [`MARKER`]) anywhere but its install folder: install
/// it there by [`placement`]'s rules and start the installed copy. Returns the exit code when this
/// launch is done; None when it should run as usual.
pub fn place() -> Option<i32> {
    let here = crate::install::installed_app()?;
    let ours = Manifest::read(&here);
    let dest = programs_dir(&ours);
    let installed = dest.join(APP_EXE).is_file().then(|| Manifest::read(&dest));
    let step = placement(&here, &dest, &ours, installed.as_ref());
    if step == Placement::Here {
        return None;
    }
    let v = |m: &Manifest| m.version.clone().unwrap_or_else(|| "an unknown version".into());
    crate::log::info(format!("launched from {}: {step:?} ({} in {})", here.display(), installed.as_ref().map_or("nothing".into(), v), dest.display()));
    let name = &ours.name;
    let r = match step {
        Placement::Here => return None,
        Placement::StartInstalled => start_installed(&dest),
        Placement::Newer(have) => Err(format!(
            "{name} {have} is already installed in {}, which is newer than this {}. To install this one instead, uninstall {name} first (Settings ▸ Apps ▸ {name} ▸ Uninstall), then run this again.",
            dest.display(),
            v(&ours)
        )),
        Placement::Other(id) => Err(format!("{} holds another app ({id}), so {name} won't install there.", dest.display())),
        Placement::Install | Placement::Update => foreign_files(&here, name)
            .and_then(|()| {
                // An app or daemon running from the old files would hold them, or start the old daemon again.
                if installed.is_some() && !crate::win::shut_down(&dest, &ours) {
                    return Err(format!("{name} is still running from {}. Quit it, then run this again.", dest.display()));
                }
                Ok(())
            })
            .and_then(|()| copy_in(&here, &dest).map_err(|e| format!("Couldn't install {name} in {}: {e}", dest.display())))
            .and_then(|()| {
                crate::log::info(format!("installed {name} {} in {}", v(&ours), dest.display()));
                start_installed(&dest)
            }),
    };
    Some(match r {
        Ok(()) => 0,
        Err(e) => {
            tell(name, &e);
            1
        }
    })
}

fn foreign_files(dir: &Path, name: &str) -> Result<(), String> {
    let others = not_the_build(dir)?;
    if others.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = others.iter().take(5).map(String::as_str).collect();
    Err(format!(
        "{} holds files that aren't {name}'s ({}{}). Unpack the release zip into a folder of its own and run taskboard-app.exe from there.",
        dir.display(),
        shown.join(", "),
        if others.len() > shown.len() { ", …" } else { "" }
    ))
}

/// Starts the installed app, letting it take the front (this launch came from a click).
fn start_installed(dest: &Path) -> Result<(), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    keep_std_handles();
    let exe = dest.join(APP_EXE);
    crate::win::spawn_detached(Command::new(&exe).current_dir(dest)).map_err(|e| format!("Couldn't start {}: {e}", exe.display()))
}

/// Keeps this process's standard handles to itself. Rust starts every child with handle
/// inheritance on, so the installed app (which outlives this launch) would hold the pipe a script
/// reads this launch's output from, and the script would wait for its end for good.
fn keep_std_handles() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = unsafe { GetStdHandle(which) };
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

/// Says `msg` to whoever ran this: the console it was run from, else a message box (a release
/// build has no console of its own). The app log has it either way.
fn tell(title: &str, msg: &str) {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError};
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};
    crate::log::info(msg);
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    unsafe {
        // Access denied: it has a console already (a debug build).
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 || GetLastError() == ERROR_ACCESS_DENIED {
            eprintln!("taskboard-app: {msg}");
            return;
        }
        MessageBoxW(std::ptr::null_mut(), wide(msg).as_ptr(), wide(title).as_ptr(), MB_OK | MB_ICONWARNING);
    }
}

// ------------------------------------------------------------------ Start menu

/// `<name>.lnk` in the user's Start menu Programs folder.
pub fn start_menu_entry(m: &Manifest) -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs").join(format!("{}.lnk", m.name)))
}

/// Adds the install to the Start menu, or points an entry that runs another copy at this one.
pub fn add_to_start_menu(dir: &Path, m: &Manifest) -> Result<(), String> {
    let lnk = start_menu_entry(m).ok_or("no %APPDATA%")?;
    let exe = dir.join(APP_EXE);
    if points_at(&lnk, &exe) {
        return Ok(());
    }
    if let Some(parent) = lnk.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    shortcut(&lnk, &exe, dir).map_err(|e| format!("{}: {e}", lnk.display()))
}

/// Takes the install's Start menu entry away, if it runs this install.
pub fn remove_from_start_menu(dir: &Path, m: &Manifest) {
    if let Some(lnk) = start_menu_entry(m).filter(|l| points_at(l, &dir.join(APP_EXE))) {
        let _ = std::fs::remove_file(lnk);
    }
}

/// Does the shortcut at `lnk` run `exe`?
fn points_at(lnk: &Path, exe: &Path) -> bool {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink};
    use windows::core::{HSTRING, Interface};
    if !lnk.is_file() {
        return false;
    }
    // SAFETY: COM on this thread, then plain interface calls into a buffer we own.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let target = (|| -> windows::core::Result<String> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.cast::<IPersistFile>()?.Load(&HSTRING::from(lnk.as_os_str()), STGM_READ)?;
            let mut buf = [0u16; 1024];
            link.GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)?;
            Ok(String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]))
        })();
        target.is_ok_and(|t| taskboardd::util::path_key(&t) == taskboardd::util::path_key(&exe.to_string_lossy()))
    }
}

fn shortcut(lnk: &Path, exe: &Path, dir: &Path) -> windows::core::Result<()> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, Interface};
    // SAFETY: COM on this thread (already initialized is fine), then plain interface calls.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(exe.as_os_str()))?;
        link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        link.SetDescription(&HSTRING::from("The task board for your Claude Code agents"))?;
        link.cast::<IPersistFile>()?.Save(&HSTRING::from(lnk.as_os_str()), true)
    }
}

// ------------------------------------------------------------------ Settings ▸ Apps

/// The install's entry in Settings ▸ Apps (and Programs and Features): a key under
/// `HKCU\…\Uninstall` named for its id, so Taskboard Dev's never touches Taskboard's.
pub fn apps_key(m: &Manifest) -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{}", m.id)
}

enum Data {
    Text(String),
    Number(u32),
}

/// What the entry holds for the install in `dir`, `size_kb` big. Its Uninstall button runs
/// `taskboard-app.exe --uninstall`; there's nothing to modify or repair.
fn apps_values(dir: &Path, m: &Manifest, size_kb: u32) -> Vec<(&'static str, Data)> {
    let exe = dir.join(APP_EXE);
    vec![
        ("DisplayName", Data::Text(m.name.clone())),
        ("DisplayVersion", Data::Text(m.version.clone().unwrap_or_else(|| crate::updater::VERSION.into()))),
        ("Publisher", Data::Text("Morgan Hunt".into())),
        ("DisplayIcon", Data::Text(format!("{},0", exe.display()))),
        ("InstallLocation", Data::Text(dir.display().to_string())),
        ("UninstallString", Data::Text(format!("\"{}\" --uninstall", exe.display()))),
        ("QuietUninstallString", Data::Text(format!("\"{}\" --uninstall", exe.display()))),
        ("URLInfoAbout", Data::Text(crate::updater::RELEASES_PAGE.into())),
        ("NoModify", Data::Number(1)),
        ("NoRepair", Data::Number(1)),
        ("EstimatedSize", Data::Number(size_kb)),
    ]
}

fn write_values(key: &str, values: Vec<(&'static str, Data)>) -> std::io::Result<()> {
    let (k, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(key)?;
    for (name, data) in values {
        match data {
            Data::Text(s) => k.set_value(name, &s)?,
            Data::Number(n) => k.set_value(name, &n)?,
        }
    }
    Ok(())
}

/// The size of the files in `dir`, in KB.
fn size_kb(dir: &Path) -> u32 {
    fn bytes(dir: &Path) -> u64 {
        let sizes = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => bytes(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        });
        sizes.sum()
    }
    u32::try_from(bytes(dir).div_ceil(1024)).unwrap_or(u32::MAX)
}

/// Adds the install to Settings ▸ Apps, or brings its entry up to date (version, folder). A value
/// the entry holds besides these (what [`put_on_path`] added) stays.
pub fn add_to_apps(dir: &Path, m: &Manifest) -> Result<(), String> {
    let key = apps_key(m);
    write_values(&key, apps_values(dir, m, size_kb(dir))).map_err(|e| format!("{key}: {e}"))
}

/// Takes the install's entry out of Settings ▸ Apps, if it names this install.
pub fn remove_from_apps(dir: &Path, m: &Manifest) -> Result<(), String> {
    let key = apps_key(m);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let names_us = hkcu.open_subkey(&key).and_then(|k| k.get_value::<String, _>("InstallLocation")).is_ok_and(|l| same_dir(Path::new(&l), dir));
    if !names_us {
        return Ok(());
    }
    hkcu.delete_subkey_all(&key).map_err(|e| format!("{key}: {e}"))
}

// ------------------------------------------------------------------ the user's Path

/// The `tb` folder the install added to the user's `Path`, under the Settings ▸ Apps entry's key.
const PATH_RECORD: &str = "TaskboardAddedToPath";
const ENV: &str = "Environment";

/// What [`put_on_path`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OnPath {
    /// The folder is on `Path` already (the human's, or an earlier launch's).
    Already,
    /// Added now: terminals opened from now on have it.
    Added,
    /// The install added it earlier and the human took it off since: left off.
    TakenOff,
}

/// Puts `dir` (where `tb.exe` is copied) on the user's `Path` unless this process's PATH has it
/// already, recording it under the install's Settings ▸ Apps entry so uninstalling takes away
/// only what the install added, and a folder the human took off isn't put back.
pub fn put_on_path(dir: &Path, m: &Manifest) -> Result<OnPath, String> {
    if std::env::var("PATH").is_ok_and(|p| path_has(&p, dir)) {
        return Ok(OnPath::Already);
    }
    let added = add_to_path(ENV, &apps_key(m), &backslashed(dir))?;
    if added == OnPath::Added {
        broadcast();
    }
    Ok(added)
}

/// Takes the folder the install added off the user's `Path` (only that entry), and forgets it.
pub fn take_off_path(m: &Manifest) -> Result<Option<PathBuf>, String> {
    let gone = remove_from_path(ENV, &apps_key(m))?;
    if gone.is_some() {
        broadcast();
    }
    Ok(gone)
}

fn add_to_path(env: &str, record: &str, dir: &Path) -> Result<OnPath, String> {
    let (path, expand) = read_path(env)?.unwrap_or((String::new(), true));
    let Some(new) = with_dir(&path, dir) else { return Ok(OnPath::Already) };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let ours = hkcu.open_subkey(record).and_then(|k| k.get_value::<String, _>(PATH_RECORD)).is_ok_and(|r| same_entry(&r, dir));
    if ours {
        return Ok(OnPath::TakenOff);
    }
    write_path(env, &new, expand)?;
    let (k, _) = hkcu.create_subkey(record).map_err(|e| format!("{record}: {e}"))?;
    k.set_value(PATH_RECORD, &dir.display().to_string()).map_err(|e| format!("{record}: {e}"))?;
    Ok(OnPath::Added)
}

/// Takes the folder recorded under `HKCU\<record>` off the `Path` under `HKCU\<env>` (its last
/// entry naming it) and forgets it. A `Path` that can't be read is an error, and the record stays.
fn remove_from_path(env: &str, record: &str) -> Result<Option<PathBuf>, String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(record, KEY_ALL_ACCESS) else { return Ok(None) };
    let Ok(dir) = key.get_value::<String, _>(PATH_RECORD).map(PathBuf::from) else { return Ok(None) };
    let had = match read_path(env)? {
        Some((path, expand)) => match without_dir(&path, &dir) {
            Some(new) => write_path(env, &new, expand).map(|()| true)?,
            None => false,
        },
        None => false,
    };
    key.delete_value(PATH_RECORD).map_err(|e| format!("{record}: {e}"))?;
    Ok(had.then_some(dir))
}

/// The `Path` under `HKCU\<env>` as stored, and whether it's a `REG_EXPAND_SZ`; None when there's
/// none. One that isn't a string is an error, never taken for an empty one and replaced.
fn read_path(env: &str) -> Result<Option<(String, bool)>, String> {
    let at = || format!(r"HKCU\{env}\Path");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey(env) {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", at())),
    };
    let v = match key.get_raw_value("Path") {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", at())),
    };
    let expand = match v.vtype {
        REG_EXPAND_SZ => true,
        REG_SZ => false,
        t => return Err(format!("{} isn't a string ({t:?})", at())),
    };
    let units: Vec<u16> = v.bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Ok(Some((String::from_utf16_lossy(&units[..end]), expand)))
}

fn write_path(env: &str, path: &str, expand: bool) -> Result<(), String> {
    let at = || format!(r"HKCU\{env}\Path");
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(env).map_err(|e| format!("{}: {e}", at()))?;
    let bytes: Vec<u8> = path.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
    let value = winreg::RegValue { bytes: bytes.into(), vtype: if expand { REG_EXPAND_SZ } else { REG_SZ } };
    key.set_raw_value("Path", &value).map_err(|e| format!("{}: {e}", at()))
}

/// Tells running programs the environment changed (`WM_SETTINGCHANGE` "Environment"), so Explorer,
/// and what starts from it (a new PowerShell window), picks up the new `Path`. One that doesn't
/// answer within a second is skipped.
fn broadcast() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};
    let what: Vec<u16> = "Environment".encode_utf16().chain(std::iter::once(0)).collect();
    let mut result = 0usize;
    unsafe { SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, what.as_ptr() as isize, SMTO_ABORTIFHUNG, 1000, &mut result) };
}

/// `path` (`;`-separated) with `dir` added at the end; None when one of its entries already is
/// `dir`. The other entries stay exactly as they were.
pub fn with_dir(path: &str, dir: &Path) -> Option<String> {
    if path.split(';').any(|e| same_entry(e, dir)) {
        return None;
    }
    let dir = dir.display().to_string();
    Some(match path.trim_end_matches(';') {
        "" => dir,
        rest => format!("{rest};{dir}"),
    })
}

/// `path` without its last entry that is `dir`; None when none is.
pub fn without_dir(path: &str, dir: &Path) -> Option<String> {
    let mut entries: Vec<&str> = path.split(';').collect();
    let i = entries.iter().rposition(|e| same_entry(e, dir))?;
    entries.remove(i);
    Some(entries.join(";"))
}

/// Does the `Path` entry `entry` name `dir`? Its `%NAME%`s expanded and quotes dropped, without
/// case, either slash, a trailing one.
pub fn same_entry(entry: &str, dir: &Path) -> bool {
    let entry = expand(entry.trim().trim_matches('"'));
    !entry.is_empty() && taskboardd::util::path_key(&entry) == taskboardd::util::path_key(&dir.to_string_lossy())
}

/// `dir` as Windows writes a folder: `~/.local/bin` expands to `C:\Users\me/.local/bin`.
fn backslashed(dir: &Path) -> PathBuf {
    PathBuf::from(dir.to_string_lossy().replace('/', "\\"))
}

/// Is `dir` one of `path`'s folders?
fn path_has(path: &str, dir: &Path) -> bool {
    path.split(';').any(|e| same_entry(e, dir))
}

/// `s` with each `%NAME%` of a set variable replaced by its value.
fn expand(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        let after = &rest[i + 1..];
        match after.find('%').and_then(|j| std::env::var(&after[..j]).ok().map(|v| (j, v))) {
            Some((j, v)) => {
                out.push_str(&rest[..i]);
                out.push_str(&v);
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[..=i]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ------------------------------------------------------------------ removing the install

/// Hands the install's files to a copy of this app in `%TEMP%`, which deletes them once this
/// process (`--uninstall`, running from them) has exited ([`remove_main`]).
pub fn remove_later(dir: &Path) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let helper = std::env::temp_dir().join(format!("taskboard-uninstall-{}.exe", std::process::id()));
    std::fs::copy(&me, &helper).map_err(|e| format!("{}: {e}", helper.display()))?;
    let mut cmd = Command::new(&helper);
    cmd.arg("--remove-install").arg(dir).arg("--wait").arg(std::process::id().to_string()).current_dir(std::env::temp_dir());
    crate::win::spawn_detached(&mut cmd).map_err(|e| format!("{}: {e}", helper.display()))
}

/// `taskboard-app.exe --remove-install <folder> --wait <pid>`, run from a copy in `%TEMP%`: waits
/// for the uninstall to exit, deletes the install ([`remove_install`]), then has itself deleted.
pub fn remove_main(args: &[String]) -> i32 {
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let (Some(dir), Some(pid)) = (value("--remove-install"), value("--wait").and_then(|p| p.parse::<u32>().ok())) else {
        crate::log::info("usage: taskboard-app --remove-install <folder> --wait <pid>");
        return 2;
    };
    crate::win::wait_exit(pid, Duration::from_secs(60));
    let code = match remove_install(Path::new(&dir)) {
        Ok(left) if left.is_empty() => {
            crate::log::info(format!("removed {dir}"));
            0
        }
        Ok(left) => {
            crate::log::info(format!("removed the install from {dir}; left what isn't Taskboard's: {}", left.join(", ")));
            0
        }
        Err(e) => {
            crate::log::info(format!("removing {dir}: {e}"));
            1
        }
    };
    // A running program can't delete itself: cmd does once this one has gone.
    if let Ok(me) = std::env::current_exe() {
        let line = format!("/c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"", me.display());
        let _ = Command::new("cmd.exe").raw_arg(line).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).spawn();
    }
    code
}

/// Deletes the install in `dir`: the release's files and folders, and what updates and installs
/// left (`.update-*`, `.old-*`, `.install-*`), the marker last, then `dir` once it's empty.
/// Refuses a folder that isn't an install. Returns the names left because they aren't Taskboard's.
pub fn remove_install(dir: &Path) -> Result<Vec<String>, String> {
    if !dir.join(MARKER).is_file() || !dir.join(APP_EXE).is_file() {
        return Err(format!("{} isn't an installed Taskboard", dir.display()));
    }
    let mut failed = vec![];
    let mut left = vec![];
    for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
        let (p, name) = (e.path(), e.file_name().to_string_lossy().to_string());
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        let leftover = [".update-", ".old-", ".install-"].iter().any(|s| name.starts_with(s));
        let ours = leftover || if is_dir { RELEASE_DIRS.iter().any(|d| d.eq_ignore_ascii_case(&name)) } else { RELEASE_FILES.iter().any(|f| f.eq_ignore_ascii_case(&name)) };
        if !ours {
            left.push(name);
            continue;
        }
        if name.eq_ignore_ascii_case(MARKER) {
            continue;
        }
        let gone = retry(|| if is_dir { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) });
        if gone.is_err() {
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        return Err(format!("couldn't remove {} (in use?)", failed.join(", ")));
    }
    retry(|| std::fs::remove_file(dir.join(MARKER))).map_err(|e| format!("{}: {e}", dir.join(MARKER).display()))?;
    if left.is_empty() {
        retry(|| std::fs::remove_dir(dir)).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(left)
}

/// Tries `f` for up to 10 seconds: a program that just exited can hold its file a moment longer.
fn retry(f: impl Fn() -> std::io::Result<()>) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    loop {
        match f() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if start.elapsed() > Duration::from_secs(10) => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
            Ok(()) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("taskboard-selfinstall-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn manifest(id: &str, version: Option<&str>) -> Manifest {
        Manifest::parse(&json!({"id": id, "version": version}))
    }

    /// A release folder as build.sh makes it, its programs holding `marker`.
    fn release(dir: &Path, version: &str, marker: &str) {
        std::fs::create_dir_all(dir.join(r"plugin\task-board\bin")).unwrap();
        std::fs::create_dir_all(dir.join("Fonts")).unwrap();
        for f in [APP_EXE, DAEMON_EXE, "tb.exe", r"plugin\task-board\bin\tb", r"Fonts\OFL-Inter.txt"] {
            std::fs::write(dir.join(f), marker).unwrap();
        }
        std::fs::write(dir.join(MANIFEST), json!({"id": "com.mrgnhnt.taskboard", "version": version}).to_string()).unwrap();
        std::fs::write(dir.join(MARKER), format!("Taskboard {version}")).unwrap();
    }

    #[test]
    fn a_release_folder_installs_starts_updates_or_refuses_by_what_s_installed() {
        let d = scratch("placement");
        let (here, dest) = (d.join(r"Downloads\Taskboard"), d.join(r"Programs\Taskboard"));
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let ours = manifest("com.mrgnhnt.taskboard", Some("0.1.0-beta.97.w.2"));
        let at = |v: Option<&str>| manifest("com.mrgnhnt.taskboard", v);
        assert_eq!(placement(&here, &dest, &ours, None), Placement::Install);
        assert_eq!(placement(&here, &dest, &ours, Some(&at(Some("0.1.0-beta.97.w.2")))), Placement::StartInstalled);
        assert_eq!(placement(&here, &dest, &ours, Some(&at(Some("0.1.0-beta.97.w.1")))), Placement::Update);
        assert_eq!(placement(&here, &dest, &ours, Some(&at(None))), Placement::Update, "an install whose version can't be read is replaced");
        assert_eq!(placement(&here, &dest, &ours, Some(&at(Some("0.1.0-beta.98.w.1")))), Placement::Newer("0.1.0-beta.98.w.1".into()));
        assert_eq!(placement(&here, &dest, &ours, Some(&manifest("com.mrgnhnt.taskboard.dev", Some("1.0.0")))), Placement::Other("com.mrgnhnt.taskboard.dev".into()));
        let spelled = PathBuf::from(dest.to_string_lossy().to_uppercase().replace('\\', "/") + "/");
        assert_eq!(placement(&dest, &spelled, &ours, Some(&ours)), Placement::Here, "the install folder runs as usual, however it's spelled");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_with_anything_but_the_release_isn_t_installed_from() {
        let d = scratch("foreign");
        release(&d, "1.0.0", "x");
        assert!(not_the_build(&d).unwrap().is_empty());
        std::fs::write(d.join("taxes.pdf"), "").unwrap();
        std::fs::create_dir_all(d.join("Photos")).unwrap();
        assert_eq!(not_the_build(&d).unwrap(), vec!["Photos", "taxes.pdf"]);
        let why = foreign_files(&d, "Taskboard").unwrap_err();
        assert!(why.contains("taxes.pdf") && why.contains("folder of its own"), "{why}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copying_in_installs_fresh_and_replaces_an_older_install_file_by_file() {
        let d = scratch("copy-in");
        let (v1, v2, dest) = (d.join("v1"), d.join("v2"), d.join(r"Programs\Taskboard"));
        release(&v1, "1.0.0", "old");
        std::fs::write(v1.join(r"plugin\task-board\bin\gone"), "old").unwrap();
        release(&v2, "1.1.0", "new");
        // Marked as downloaded, as Explorer's Extract All leaves a zip's files.
        let mut stream = v2.join(APP_EXE).into_os_string();
        stream.push(":Zone.Identifier");
        std::fs::write(&stream, "[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();

        copy_in(&v1, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join(APP_EXE)).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(dest.join(MARKER)).unwrap(), "Taskboard 1.0.0");
        std::fs::write(dest.join("app.log"), "kept").unwrap();

        copy_in(&v2, &dest).unwrap();
        for f in [APP_EXE, DAEMON_EXE, "tb.exe", r"plugin\task-board\bin\tb", r"Fonts\OFL-Inter.txt"] {
            assert_eq!(std::fs::read_to_string(dest.join(f)).unwrap(), "new", "{f}");
        }
        assert_eq!(Manifest::read(&dest).version.as_deref(), Some("1.1.0"));
        assert_eq!(std::fs::read_to_string(dest.join(MARKER)).unwrap(), "Taskboard 1.1.0");
        assert!(!dest.join(r"plugin\task-board\bin\gone").exists(), "the plugin is the new one, whole");
        assert_eq!(std::fs::read_to_string(dest.join("app.log")).unwrap(), "kept");
        let mut copied = dest.join(APP_EXE).into_os_string();
        copied.push(":Zone.Identifier");
        assert!(std::fs::metadata(PathBuf::from(copied)).is_err(), "the installed copy isn't marked as downloaded");
        assert!(std::fs::metadata(PathBuf::from(stream)).is_ok(), "the release folder keeps its mark");
        let stray: Vec<String> = std::fs::read_dir(&dest).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with('.')).collect();
        assert!(stray.is_empty(), "nothing staged is left: {stray:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn removing_the_install_takes_only_taskboard_s_files() {
        let d = scratch("remove");
        let dest = d.join("Taskboard");
        release(&dest, "1.0.0", "x");
        std::fs::create_dir_all(dest.join(".old-12")).unwrap();
        std::fs::write(dest.join("notes.txt"), "mine").unwrap();
        assert_eq!(remove_install(&dest).unwrap(), vec!["notes.txt"]);
        let left: Vec<String> = std::fs::read_dir(&dest).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(left, vec!["notes.txt"]);
        std::fs::remove_file(dest.join("notes.txt")).unwrap();
        assert!(remove_install(&dest).is_err(), "no longer an install");
        release(&dest, "1.0.0", "x");
        assert!(remove_install(&dest).unwrap().is_empty());
        assert!(!dest.exists(), "an emptied folder goes too");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn path_entries_are_added_once_and_only_the_one_added_is_taken_off() {
        let dir = Path::new(r"C:\Users\me\.local\bin");
        assert_eq!(with_dir("", dir).as_deref(), Some(r"C:\Users\me\.local\bin"));
        assert_eq!(with_dir(r"C:\a;", dir).as_deref(), Some(r"C:\a;C:\Users\me\.local\bin"));
        assert_eq!(with_dir(r"C:\a;c:/users/ME/.local/bin/", dir), None, "already there, however it's spelled");
        assert_eq!(without_dir(r"C:\a;C:\Users\me\.local\bin;C:\b", dir).as_deref(), Some(r"C:\a;C:\b"));
        assert_eq!(without_dir(r"C:\a;C:\b", dir), None);
        // SAFETY: a variable only this test reads.
        unsafe { std::env::set_var("TASKBOARD_TEST_PROFILE", r"C:\Users\me") };
        assert!(same_entry(r#""%TASKBOARD_TEST_PROFILE%\.local\bin""#, dir));
        assert!(!same_entry(r"%NOT_SET_ANYWHERE%\.local\bin", dir));
        assert_eq!(backslashed(Path::new(r"C:\Users\me/.local/bin")), dir, "written the way Windows writes it");
    }

    #[test]
    fn the_user_path_keeps_its_type_and_a_folder_taken_off_stays_off() {
        // Scratch keys stand in for HKCU\Environment and the Settings ▸ Apps entry.
        let env = format!(r"Software\taskboard-test-env-{}", std::process::id());
        let record = format!(r"Software\taskboard-test-apps-{}", std::process::id());
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let dir = Path::new(r"C:\Users\me\.local\bin");
        write_path(&env, r"%USERPROFILE%\bin;C:\tools", true).unwrap();

        assert_eq!(add_to_path(&env, &record, dir).unwrap(), OnPath::Added);
        assert_eq!(read_path(&env).unwrap(), Some((r"%USERPROFILE%\bin;C:\tools;C:\Users\me\.local\bin".to_string(), true)), "still a REG_EXPAND_SZ");
        assert_eq!(add_to_path(&env, &record, dir).unwrap(), OnPath::Already);

        write_path(&env, r"%USERPROFILE%\bin;C:\tools", true).unwrap();
        assert_eq!(add_to_path(&env, &record, dir).unwrap(), OnPath::TakenOff, "the human took it off: left off");

        write_path(&env, r"C:\tools;C:\Users\me\.local\bin;C:\more", false).unwrap();
        assert_eq!(remove_from_path(&env, &record).unwrap(), Some(dir.to_path_buf()));
        assert_eq!(read_path(&env).unwrap(), Some((r"C:\tools;C:\more".to_string(), false)), "only that entry, and the type kept");
        assert_eq!(remove_from_path(&env, &record).unwrap(), None, "forgotten once taken off");

        let k = hkcu.open_subkey_with_flags(&env, KEY_ALL_ACCESS).unwrap();
        k.set_value("Path", &7u32).unwrap();
        assert!(add_to_path(&env, &record, dir).is_err(), "a Path that isn't a string is never replaced");
        let _ = hkcu.delete_subkey_all(&env);
        let _ = hkcu.delete_subkey_all(&record);
    }

    #[test]
    fn the_apps_entry_uninstalls_with_the_app_and_has_no_modify_or_repair() {
        let d = scratch("apps");
        std::fs::write(d.join(APP_EXE), vec![0u8; 3000]).unwrap();
        std::fs::create_dir_all(d.join("Fonts")).unwrap();
        std::fs::write(d.join(r"Fonts\a.txt"), vec![0u8; 2000]).unwrap();
        assert_eq!(size_kb(&d), 5);
        let m = manifest("com.mrgnhnt.taskboard.dev", Some("1.2.0"));
        assert_eq!(apps_key(&m), r"Software\Microsoft\Windows\CurrentVersion\Uninstall\com.mrgnhnt.taskboard.dev");
        let key = format!(r"Software\taskboard-test-uninstall-{}", std::process::id());
        write_values(&key, apps_values(&d, &m, 5)).unwrap();
        let k = RegKey::predef(HKEY_CURRENT_USER).open_subkey(&key).unwrap();
        let s = |n: &str| k.get_value::<String, _>(n).unwrap();
        assert_eq!(s("UninstallString"), format!("\"{}\" --uninstall", d.join(APP_EXE).display()));
        assert_eq!((s("DisplayName").as_str(), s("DisplayVersion").as_str()), ("Taskboard", "1.2.0"));
        assert_eq!(k.get_value::<u32, _>("NoModify").unwrap(), 1);
        let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&key);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_start_menu_shortcut_runs_the_app_from_its_folder() {
        let d = scratch("lnk");
        let (dir, other) = (d.join("Taskboard"), d.join("Elsewhere"));
        let lnk = d.join("Taskboard.lnk");
        assert!(!points_at(&lnk, &dir.join(APP_EXE)));
        shortcut(&lnk, &dir.join(APP_EXE), &dir).unwrap();
        assert!(points_at(&lnk, &dir.join(APP_EXE)));
        assert!(!points_at(&lnk, &other.join(APP_EXE)));
        let _ = std::fs::remove_dir_all(&d);
    }
}
