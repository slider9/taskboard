//! On Windows, links `app.rc` into the app: the icon its windows, the taskbar and Explorer show.

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=../../packaging/assets/Taskboard.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("app.rc", embed_resource::NONE).manifest_optional().expect("app.rc compiles");
    }
}
