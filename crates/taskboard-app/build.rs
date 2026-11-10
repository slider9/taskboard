//! On Windows, links the app icon into the app: the one its windows, the taskbar and Explorer show.
//! `TASKBOARD_ICON=<file.ico>` picks another (Taskboard Dev's, from `packaging/windows/build.sh`).

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../../packaging/assets/Taskboard.ico");
    println!("cargo:rerun-if-env-changed=TASKBOARD_ICON");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let crate_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let icon = std::env::var_os("TASKBOARD_ICON").map(PathBuf::from).unwrap_or_else(|| crate_dir.join("../../packaging/assets/Taskboard.ico"));
    let icon = icon.canonicalize().unwrap_or(icon);
    println!("cargo:rerun-if-changed={}", icon.display());
    // GPUI's Windows backend gives every window icon resource 1.
    let rc = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("app.rc");
    let path = icon.display().to_string().trim_start_matches(r"\\?\").replace('\\', r"\\");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).expect("app.rc written");
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().expect("app.rc compiles");
}
