; Inno Setup script for the OxiMux Windows installer.
;
; This compiles dist\OxiMux\ -- whatever scripts\bundle-windows.ps1 just
; assembled and asserted -- into a single OxiMux-<version>-x64-setup.exe.
;
; It deliberately knows nothing about the layout it is packaging. The bundle
; script is the thing that decides which files ship and fails when one is
; missing (a build without oximux-screen-gate.exe runs every agent chat
; unenforced and otherwise looks normal); duplicating that list here would give
; it a second, silently divergent copy. So this takes the directory wholesale.
;
; Invoke via the bundle script, not directly:
;
;   ./scripts/bundle-windows.ps1 -Installer
;
; which is what supplies AppVersion and SourceDir below.
;
; NOTE: pure ASCII, and it has to stay that way. Inno Setup reads a .iss with no
; byte-order mark as ANSI, so a UTF-8 em dash here becomes three characters --
; harmless in a comment, corruption in a [Messages] string.

#ifndef AppVersion
  #error AppVersion is not defined. Run scripts/bundle-windows.ps1 -Installer instead of iscc directly.
#endif
#ifndef SourceDir
  #error SourceDir is not defined. Run scripts/bundle-windows.ps1 -Installer instead of iscc directly.
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

#define AppName      "OxiMux"
#define AppPublisher "nhtera"
#define AppUrl       "https://github.com/nhtera/OxiMux"
#define AppExeName   "oximux.exe"
; Must stay in lockstep with APP_DATA_SUBDIR in apps/desktop/src/app_paths.rs.
; Only the uninstaller's "also remove my data" branch reads it, so a drift here
; fails in the one place nobody tests -- hence naming the file that owns it.
#define AppDataSubdir "dev.nhtera.oximux"

[Setup]
; Never change AppId. It is the identity Windows matches an upgrade against;
; a new one turns every future release into a second parallel installation
; with its own Add/Remove entry.
AppId={{558F5659-8AD6-40A0-B0A8-CA149FD059C9}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#AppVersion}

; Per-user, and not for want of ambition. Two reasons, in order:
;
; 1. crates/auto-update has no Windows pipeline yet, and when it grows one it
;    has to replace oximux.exe and its siblings in place. A per-user directory
;    it can write unelevated is the only shape that works without shipping an
;    elevated helper service purely to copy files.
; 2. Nothing here needs machine scope. Every writable path OxiMux touches is
;    under %LOCALAPPDATA% already (apps/desktop/src/app_paths.rs), so a
;    per-machine install would buy an admin prompt on every upgrade and change
;    nothing else.
;
; PrivilegesRequiredOverridesAllowed is deliberately left at its default of
; nothing rather than set explicitly, so no override is offered: the elevation
; dialog would let a user pick Program Files and quietly opt out of (1).
PrivilegesRequired=lowest
; {autopf} under `lowest` is %LOCALAPPDATA%\Programs.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExeName}

; x64compatible rather than x64: it also covers ARM64 running x64 under
; emulation, which is the only way an ARM64 machine runs this build today
; (release.yml builds x86_64-pc-windows-msvc only).
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; GPUI's Windows backend needs DirectComposition; there is no Windows 8 story
; to degrade to, so refuse early rather than fail at first paint.
MinVersion=10.0

SetupIconFile=..\..\assets\windows\OxiMux.ico
LicenseFile=..\..\LICENSE
WizardStyle=modern
OutputDir={#OutputDir}
OutputBaseFilename={#AppName}-{#AppVersion}-x64-setup
Compression=lzma2/max
SolidCompression=yes

; The relay daemon outlives the app on purpose, so an upgrade that only looked
; for oximux.exe would find the directory busy anyway: Windows refuses to
; replace a mapped image, and left to itself that surfaces as "cannot write
; onnxruntime.dll" -- a file nobody touched. Restart Manager closes both, and
; the app respawns its relay on next launch.
CloseApplications=yes
CloseApplicationsFilter=*.exe,*.dll
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Recursive and unfiltered, for the reason in the header: the bundle script owns
; the manifest. `ignoreversion` because these are our own binaries -- version
; comparison would skip a rebuilt exe whose VERSIONINFO did not change, which is
; every build between releases.
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Files the app writes into its own directory are not supposed to exist -- see
; app_paths.rs -- but a crash dump or a log dropped beside the exe would leave
; the directory behind and the uninstall looking half-done.
Type: filesandordirs; Name: "{app}"

[Code]
function UserDataDir(): String;
begin
  Result := ExpandConstant('{localappdata}\{#AppDataSubdir}');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Dir: String;
begin
  if CurUninstallStep <> usPostUninstall then
    Exit;

  Dir := UserDataDir();
  if not DirExists(Dir) then
    Exit;

  { Asked, never assumed. This directory is oximux.db -- every transcript of
    every project -- plus session snapshots and any speech models downloaded
    since, which are hundreds of megabytes nobody wants to fetch twice. An
    uninstall is also how a reinstall starts, so the safe default is to keep it.

    SuppressibleMsgBox rather than MsgBox, and IDNO rather than IDYES: a
    /SILENT uninstall must not block on a dialog nobody can see, and when it
    cannot ask, keeping the data is the answer that loses nothing. MB_DEFBUTTON2
    puts No under the cursor for the interactive case too. }
  if SuppressibleMsgBox('Also delete OxiMux settings, session history and downloaded speech models?'
            + #13#10 + #13#10 + Dir
            + #13#10 + #13#10 + 'Choose No to keep them for a future reinstall.',
            mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES then
    DelTree(Dir, True, True, True);
end;
