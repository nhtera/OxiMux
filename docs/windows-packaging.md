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
./scripts/bundle-windows.ps1 -Target x86_64-pc-windows-msvc -Zip -Installer
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
`#![cfg_attr(windows, windows_subsystem = "windows")]` — every build, debug
included.

Debug builds used to stay console-subsystem so `cargo run` had somewhere to
print. The cost surfaced once the port was actually used: a console-subsystem
binary launched outside an existing console (Explorer, the debugger, a Start
menu pin) makes Windows conjure one, and on a machine whose default terminal
is Windows Terminal that console is a *persistent* empty "Terminal" window —
one per launch, left behind as a dead pane when the app exits.

Debug `main` now calls `AttachConsole(ATTACH_PARENT_PROCESS)` first thing
instead: launched from a console, the process joins it (logs land there,
Ctrl+C works); launched from anywhere else the call fails and no window ever
exists. This is Zed's recipe — their `AttachConsole` sits behind an explicit
`--foreground` flag because attaching ties the app's life to the launching
terminal, which is right for a dev run and wrong for a release app started
from a shell; OxiMux gates on build profile until it grows a CLI surface.
The release consequence is unchanged and worth stating plainly: **a release
build has no stdout anywhere**. Anything that has to survive a packaged run
must reach a file, not `eprintln!`.

## The installer

`-Installer` compiles `packaging/windows/oximux.iss` with
[Inno Setup](https://jrsoftware.org/isinfo.php) into
`dist/OxiMux-<version>-x64-setup.exe`. A release carries both it and the zip,
because they answer different questions — "let me try this" versus "put it in my
Start menu and let me uninstall it later" — from one payload directory, so they
cannot disagree about what shipped.

```powershell
winget install JRSoftware.InnoSetup     # 6.3+; preinstalled on windows-latest
```

The `.iss` does not know the file list. `bundle-windows.ps1` owns it, asserts it,
and hands over `dist/OxiMux` wholesale; a second copy of the manifest here is a
second copy that can silently disagree about `oximux-screen-gate.exe`.

Three choices in it are worth the words:

**Per-user, into `%LOCALAPPDATA%\Programs\OxiMux`** (`PrivilegesRequired=lowest`,
with no override offered). Not modesty — `crates/auto-update` has no Windows
pipeline yet, and when it grows one it has to replace `oximux.exe` and its
siblings in place. A directory it can write unelevated is the only shape that
works without shipping an elevated helper service purely to copy files. Machine
scope would buy a UAC prompt on every upgrade and nothing else, because every
writable path is under `%LOCALAPPDATA%` already (`apps/desktop/src/app_paths.rs`).

**`CloseApplications=yes`.** The relay daemon outlives the app on purpose, so an
upgrade that only looked for `oximux.exe` would still find the directory busy:
Windows refuses to replace a mapped image, and left alone that surfaces as
"cannot write `onnxruntime.dll`" — a file nobody touched. This is the same
failure `bundle-windows.ps1` guards with its `Get-Process` check, in the one
place a user meets it.

**The uninstaller asks before deleting `%LOCALAPPDATA%\dev.nhtera.oximux`**, and
defaults to keeping it. That directory is `oximux.db` — every transcript of every
project — plus session snapshots and any downloaded speech models, which are
hundreds of megabytes nobody wants to fetch twice. An uninstall is also how a
reinstall starts.

Never change `AppId`. It is the identity Windows matches an upgrade against; a
new one turns every future release into a second parallel installation with its
own Add/Remove entry.

## Signing

There is none. Windows code signing requires a certificate issued by a CA — it
cannot be self-issued the way an ad-hoc macOS signature can — so both release
artifacts are unsigned and SmartScreen will warn on first run.

**The installer does not change this.** A `setup.exe` is a packaging
convenience, not a trust story: SmartScreen warns on it exactly as it warns on
an `oximux.exe` extracted from the zip.

That is deliberate rather than unfinished. A self-signed binary would *look*
signed while still triggering the same warning, which is worse than being
plainly unsigned. Adding real signing later means running `signtool sign /fd
sha256 /tr <timestamp-url>` over each `.exe`, innermost first, before the zip is
made — and over the payload *before* `iscc` reads it as well as over the
`setup.exe` after, since signing the wrapper alone leaves every installed binary
unsigned.

Note the asymmetry with the driver-trust story in
`docs/windows-port-exclusions.md`: OxiMux asks users to pin an unsigned
third-party binary by hash, and ships unsigned itself. Both follow from the same
platform fact, and neither is a reason to overstate the other.

## CI

`release.yml` has a `release-windows` job that builds, packages, and attaches
both `OxiMux-<version>-windows-x64.zip` and `OxiMux-<version>-x64-setup.exe` to
the draft release. It does not depend on the macOS job: notarization can stall
for an hour, and there is no reason a Windows artifact should wait behind that.
Both jobs may therefore try to create the draft release, so whichever loses that
race falls through to uploading into the one that already exists. The upload step
requires *both* files — a release that quietly carries only the zip is exactly
the failure a user following a "download the installer" link would hit.

`ci.yml`'s `windows-check` job parse-checks `install-cli.ps1` and
`bundle-windows.ps1`, and *compiles* `oximux.iss` against a stand-in payload.
Inno Setup has no syntax-only mode, and the `[Code]` section is Pascal that
nothing else here would look at; without this step a typo in either file first
surfaces after a tag, at the end of an hour-long build.

`ci.yml` runs `xtask icon --check` on the macOS job — the icon goes stale on the
machine that edits the `.icns`, which is not the Windows runner.
