; Taskboard's Windows installer (Inno Setup 6). packaging/windows/build.sh --installer compiles it
; with the install folder it assembled:
;
;   ISCC /DVersion=1.2.0 /DFileVersion=1.2.0 /DName=Taskboard /DSource=<folder> /DOutputDir=<dir>
;        /DOutputName=<file> packaging\windows\Taskboard.iss
;
; Per user, no admin: %LOCALAPPDATA%\Programs\Taskboard, a Start menu entry, and an entry in
; Settings > Apps to uninstall it. The app registers the rest itself each time it opens (the startup
; entry that runs the daemon, taskboard:// links; crates/taskboard-app/src/win.rs), and
; `taskboard-app.exe --uninstall` takes them out again. The board's data (~\.config\taskboard) stays.

#ifndef Version
  #error Pass /DVersion=<version>
#endif
#ifndef FileVersion
  #define FileVersion Version
#endif
#ifndef Name
  #define Name "Taskboard"
#endif
#ifndef Source
  #error Pass /DSource=<the install folder build.sh made>
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef OutputName
  #define OutputName Name + "-" + Version + "-windows-x64-setup"
#endif

[Setup]
AppId={{303DB09F-E888-4ABA-A31F-BE3C85F44E34}
AppName={#Name}
AppVersion={#Version}
AppVerName={#Name} {#Version}
AppPublisher=Morgan Hunt
AppPublisherURL=https://github.com/mrgnhnt96/taskboard
AppSupportURL=https://github.com/mrgnhnt96/taskboard/issues
AppUpdatesURL=https://github.com/mrgnhnt96/taskboard/releases
VersionInfoVersion={#FileVersion}
; Per user ({autopf} is %LOCALAPPDATA%\Programs), where the app and its hooks expect it.
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#Name}
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1803: AF_UNIX sockets (Midna, the app's links) and the tar.exe the updater unpacks with.
MinVersion=10.0.17134
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
SetupIconFile=..\assets\Taskboard.ico
UninstallDisplayIcon={app}\taskboard-app.exe
UninstallDisplayName={#Name}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; PrepareToInstall stops the app and the daemon itself.
CloseApplications=no

[Files]
Source: "{#Source}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[InstallDelete]
; The plugin is replaced whole, so a file an older version had doesn't linger.
Type: filesandordirs; Name: "{app}\plugin"

[Icons]
Name: "{autoprograms}\{#Name}"; Filename: "{app}\taskboard-app.exe"

[Run]
; The daemon again (PrepareToInstall stopped it), even if the app isn't opened now.
Filename: "{app}\taskboard-app.exe"; Parameters: "--daemon"; Flags: nowait runhidden
Filename: "{app}\taskboard-app.exe"; Description: "Open {#Name}"; Flags: nowait postinstall

[UninstallRun]
; Quits the app, stops the daemon, and takes out the startup entry, the link scheme, the tb copy in
; ~\.local\bin and Claude Code's hooks.
Filename: "{app}\taskboard-app.exe"; Parameters: "--uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "Uninstall"

[UninstallDelete]
Type: filesandordirs; Name: "{app}\plugin"
Type: filesandordirs; Name: "{app}\.update"
Type: filesandordirs; Name: "{app}\.old-*"

[Code]
// An open Taskboard and its daemon run from the files about to be replaced: stop them first.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
  Exe: String;
begin
  Result := '';
  Exe := ExpandConstant('{app}\taskboard-app.exe');
  if FileExists(Exe) then
    Exec(Exe, '--stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
