; Inno Setup 6 script for the GitDragon Windows installer (per-user by default, no admin needed).
; Build (after `cargo build --release --locked -p gitdragon`):
;   powershell -File packaging/windows/build.ps1            (finds iscc.exe itself)
; or directly:
;   iscc /DAppVersion=0.1.0 /DSourceExe=..\..\target\release\gitdragon.exe /DOutDir=..\..\dist packaging\windows\gitdragon.iss
; Install Inno Setup once with: winget install JRSoftware.InnoSetup

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\gitdragon.exe"
#endif
#ifndef SourceMcp
  #define SourceMcp "..\..\target\release\gitdragon-mcp.exe"
#endif
#ifndef OutDir
  #define OutDir "..\..\dist"
#endif

[Setup]
; Never change AppId: it is how upgrades and uninstall find the previous install.
AppId={{872E9A10-134D-43F3-9B18-8F31238A2CE5}
AppName=GitDragon
AppVersion={#AppVersion}
AppVerName=GitDragon {#AppVersion}
AppPublisher=InfernoDragon0
AppPublisherURL=https://github.com/InfernoDragon0/GitDragon
DefaultDirName={autopf}\GitDragon
DefaultGroupName=GitDragon
DisableProgramGroupPage=yes
; Per-user install unless the user picks "all users" in the dialog.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutDir}
OutputBaseFilename=GitDragon-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=GitDragon
UninstallDisplayIcon={app}\gitdragon.exe
; TODO(icon): add SetupIconFile=..\..\assets\gitdragon.ico once an .ico is generated from the logo.
; TODO(signing): enable once an Authenticode certificate exists (see docs/release.md), and define the
; tool on the iscc command line: /Ssigntool="signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /sha1 THUMBPRINT $f"
; SignTool=signtool
; SignedUninstaller=yes

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceMcp}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\GitDragon"; Filename: "{app}\gitdragon.exe"
Name: "{autodesktop}\GitDragon"; Filename: "{app}\gitdragon.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\gitdragon.exe"; Description: "Launch GitDragon"; Flags: nowait postinstall skipifsilent

; User data (settings, account list, caches) lives under the user profile and is left in place on uninstall.
