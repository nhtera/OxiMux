//! Embed the Windows application icon into `oximux.exe`.
//!
//! Two things read this icon and neither of them reads a file:
//!
//! * **Explorer / the taskbar / Alt-Tab** read the executable's `.rsrc`
//!   section. An icon shipped alongside the binary is an icon nobody sees.
//! * **GPUI's window** calls `LoadImageW(module, PCWSTR(1), IMAGE_ICON, …)`,
//!   which is why the resource is emitted at **ID 1** specifically. Any other
//!   id compiles, links, ships, and leaves the window with the default
//!   placeholder — with nothing anywhere saying why.
//!
//! GPUI already embeds *its* resource at ID 1: the application manifest
//! (`RT_MANIFEST`, type 24) that turns on per-monitor DPI awareness and common
//! controls. Ids are scoped per resource type, so `1 ICON` (type 14/3) and
//! `1 RT_MANIFEST` coexist rather than collide, and both are wanted.
//!
//! The `.rc` is generated into `OUT_DIR` rather than checked in so it can name
//! the icon by absolute path. A checked-in `.rc` has to spell the path relative
//! to whatever directory `rc.exe` happens to resolve from, which differs
//! between the MSVC and GNU toolchains — a portability question with no
//! upside, since the file's entire content is one line.
//!
//! The icon itself is derived from the macOS `.icns` by `cargo run -p xtask --
//! icon`; see `xtask/src/icon.rs` for why it is generated rather than drawn.

use std::path::{Path, PathBuf};

/// Repo-relative path to the checked-in Windows icon.
const ICON: &str = "assets/windows/OxiMux.ico";

fn main() {
    // Guard on the *target*, not the host: a cross-compile to Windows from a
    // macOS host still needs the resource, and a native macOS build must not
    // go looking for `rc.exe`.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );
    // apps/desktop -> apps -> repo root.
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("apps/desktop is two levels below the repo root");
    let icon = root.join(ICON);

    // A missing icon is a build failure, not a silently icon-less binary. It
    // means the checkout is incomplete or the path moved, and both are worth
    // hearing about here rather than from a user looking at a blank taskbar.
    if !icon.is_file() {
        panic!(
            "{} is missing — run `cargo run -p xtask -- icon` to generate it",
            icon.display()
        );
    }

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR"));
    let rc_path = out_dir.join("oximux.rc");

    // `.rc` string literals are C-like, so a Windows path's backslashes have to
    // be doubled or `rc.exe` reads `\O`, `\w` &c. as escapes.
    let escaped = icon.display().to_string().replace('\\', r"\\");
    std::fs::write(&rc_path, format!("1 ICON \"{escaped}\"\n"))
        .unwrap_or_else(|e| panic!("writing {}: {e}", rc_path.display()));

    // embed-resource emits no rerun-if-changed of its own (it cannot know what
    // a `.rc` includes), so without these two lines cargo falls back to
    // watching the whole crate — and a changed icon with unchanged sources
    // would not rebuild.
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=build.rs");

    // `manifest_optional`: this resource is an icon, and a toolchain with no
    // resource compiler should still produce a working — if plain — binary.
    // The manifest that *is* required rides in from GPUI, which asserts on it
    // there.
    embed_resource::compile(&rc_path, embed_resource::NONE)
        .manifest_optional()
        .expect("embedding the application icon");
}
