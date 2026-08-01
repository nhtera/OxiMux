# Packaging OxiMux for Windows

macOS ships an `.app` bundle: a directory with a required internal shape, an
`Info.plist` that names things, a code signature sealing all of it, and a
notarization ticket stapled inside. Windows has none of that. A Windows "app" is
an executable and the files that happen to sit next to it.

That difference is the whole design here, and it cuts both ways. There is no
manifest to get wrong — but there is also nothing that *notices* when a file is
missing. `scripts/bundle-windows.ps1` is where the noticing lives.

```powershell
./scripts/bundle-windows.ps1                                  # dist/OxiMux (release)
./scripts/bundle-windows.ps1 -Profile debug -SkipBuild        # inner loop
./scripts/bundle-windows.ps1 -Target x86_64-pc-windows-msvc -Zip
```

## What is in the directory, and what happens without it

Everything OxiMux locates at runtime, it locates as a **sibling of the running
executable** (`current_exe().parent()`), which is why the layout is flat and why
the script asserts rather than copies-and-hopes.

| File | Missing means |
|---|---|
| `oximux.exe` | — |
| `oximux-relay.exe` | Terminals fall back to an in-process backend that dies on quit. Sessions never survive a relaunch. |
| `oximux-screen-gate.exe` | **Every agent chat runs unenforced.** One warning in the log, otherwise indistinguishable from normal. |
| `rg.exe` | Search and Quick Open report "ripgrep missing". |
| `onnxruntime.dll`, `sherpa-onnx-*.dll`, `cargs.dll` | The app does not start at all. Windows resolves imports at load time, so there is no degraded mode. |

The screen-gate row is the one that justifies a script over a documented
checklist. It is the only entry whose absence makes the build *less safe* while
leaving it fully functional — and on Windows that matters more than on macOS,
because synthesizing input needs no permission there, so the hook is the only
thing watching. `docs/windows-port-exclusions.md` has the full argument.

The DLLs are copied by glob (`target/<profile>/*.dll`) rather than by name, so a
new native dependency rides along without an edit here. A glob can also match
nothing, quietly — hence the explicit assertion that `onnxruntime.dll` and
`sherpa-onnx-c-api.dll` landed.

## The application icon

The icon is **not** a file in the directory. It is a resource compiled into
`oximux.exe`, because that is the only place Explorer, the taskbar, and Alt-Tab
look. `apps/desktop/build.rs` embeds it at **resource ID 1** — not an arbitrary
choice: GPUI's Windows backend calls
`LoadImageW(module, PCWSTR(1), IMAGE_ICON, …)` for the window icon, so any other
id links cleanly and leaves the window showing the default placeholder.

The `.ico` itself is generated, not drawn:

```powershell
cargo run -p xtask -- icon           # regenerate assets/windows/OxiMux.ico
cargo run -p xtask -- icon --check   # fail if it is stale (CI runs this)
```

`assets/AppIcon.icns` is the single source of truth for both platforms.
Maintaining two hand-made binaries is how the platforms drift, and icon drift is
invisible until somebody looks at a taskbar — so the Windows half is derived and
the derivation is checked. `xtask/src/icon.rs` explains the frame sizes and why
the small ones are BMP while the 256 is PNG.

Because "nothing to copy" and "nothing embedded" look identical from the
packaging script, `bundle-windows.ps1` asks the loader
(`PrivateExtractIconsW`) whether the resource is actually in the binary, and
fails if it is not. The realistic cause is a build run without a resource
compiler on PATH: `embed-resource` treats that as optional and mentions it only
in the build log.

## Console window

`apps/desktop/src/main.rs` carries
`#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]`.

Release builds get no console. Debug builds keep it, because that is where
`tracing` output goes and losing it would make `cargo run` silent. The
consequence is worth stating plainly: **a packaged release build has no stdout**.
Anything that has to survive a packaged run must reach a file, not `eprintln!`.

## Signing

There is none. Windows code signing requires a certificate issued by a CA — it
cannot be self-issued the way an ad-hoc macOS signature can — so the release
artifact is an unsigned zip and SmartScreen will warn on first run.

That is deliberate rather than unfinished. A self-signed binary would *look*
signed while still triggering the same warning, which is worse than being
plainly unsigned. Adding real signing later means running `signtool sign /fd
sha256 /tr <timestamp-url>` over each `.exe`, innermost first, before the zip is
made.

Note the asymmetry with the driver-trust story in
`docs/windows-port-exclusions.md`: OxiMux asks users to pin an unsigned
third-party binary by hash, and ships unsigned itself. Both follow from the same
platform fact, and neither is a reason to overstate the other.

## CI

`release.yml` has a `release-windows` job that builds, packages, and attaches
`OxiMux-<version>-windows-x64.zip` to the draft release. It does not depend on
the macOS job: notarization can stall for an hour, and there is no reason a
Windows artifact should wait behind that. Both jobs may therefore try to create
the draft release, so whichever loses that race falls through to uploading into
the one that already exists.

`ci.yml` runs `xtask icon --check` on the macOS job — the icon goes stale on the
machine that edits the `.icns`, which is not the Windows runner.
