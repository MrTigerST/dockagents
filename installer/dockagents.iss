; Inno Setup script for the DockAgents Windows installer.
;
; Build with:
;     iscc installer\dockagents.iss
; or  iscc /DDockAgentsVersion=0.1.0-rc.1 installer\dockagents.iss
;
; The release workflow passes:
;   /DDockAgentsVersion=<X.Y.Z>            — used for AppVersion (must be ASCII)
;   /DSourceBinary=target\...\dockagents.exe — path to the built binary
;   /DOutputDir=dist                         — where to drop the wizard
;   /DOutputBaseName=dockagents-setup-windows-x86_64
;
; What this installer does at runtime:
;   1. Per-user install at %LOCALAPPDATA%\DockAgents (no UAC prompt).
;   2. Optional: add the install dir to the user's PATH (checked by default).
;   3. Registers an Apps & features entry — Windows shows "DockAgents" in
;      Settings → Apps and offers an Uninstall button that runs Inno Setup's
;      auto-generated uninstaller.
;
; The uninstaller automatically:
;   - Deletes the binary.
;   - Reverses any registry/PATH changes made by [Registry] and [Tasks].
;   - Removes its own registration from Apps & features.

#ifndef DockAgentsVersion
  #define DockAgentsVersion "0.0.0-dev"
#endif
#ifndef SourceBinary
  #define SourceBinary "..\target\x86_64-pc-windows-msvc\release\dockagents.exe"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif
#ifndef OutputBaseName
  #define OutputBaseName "dockagents-setup-windows-x86_64"
#endif

#define MyAppName        "DockAgents"
#define MyAppPublisher   "DockAgents"
#define MyAppURL         "https://dockagents.net"
#define MyAppExeName     "dockagents.exe"

[Setup]
; A stable GUID so successive versions upgrade the same install rather than
; piling up next to one another. Don't change this once shipped.
AppId={{2A3F8E6D-4B7C-4E91-9F5A-1C3D5E7F9B2C}
AppName={#MyAppName}
AppVersion={#DockAgentsVersion}
VersionInfoVersion={#DockAgentsVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/docs
AppUpdatesURL={#MyAppURL}/docs/install

; Per-user install (HKCU, no UAC). Users who want a system-wide install can
; pass /ALLUSERS=yes on the command line.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline dialog

DefaultDirName={autopf}\DockAgents
UsePreviousAppDir=yes
DisableDirPage=auto
DisableProgramGroupPage=yes
DisableReadyPage=no

OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; What shows up in Settings → Apps & features.
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "modifypath"; \
  Description: "Add DockAgents to the user &PATH (so you can run ""dockagents"" from any terminal)"; \
  GroupDescription: "PATH:"; \
  Flags: checkedonce

[Files]
Source: "{#SourceBinary}"; DestDir: "{app}"; Flags: ignoreversion

[Run]
Filename: "{app}\{#MyAppExeName}"; Parameters: "--version"; \
  Description: "Verify the install (runs ""dockagents --version"")"; \
  Flags: nowait postinstall skipifsilent

[Registry]
; Add {app} to the user PATH only when the user accepted the modifypath task
; AND the directory isn't already on PATH. The uninstaller removes the entry
; automatically because it owns the registry key it created.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
  ValueData: "{olddata};{app}"; \
  Check: NeedsAddPath('{app}'); \
  Tasks: modifypath

[Code]
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  // Guard the search with ;…; so we don't accidentally match a substring.
  Result := Pos(';' + Lowercase(Param) + ';',
                ';' + Lowercase(OrigPath) + ';') = 0;
end;
