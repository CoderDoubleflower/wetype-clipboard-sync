#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif
#ifndef PackageDir
  #define PackageDir "..\..\dist\package"
#endif
#ifndef OutputDirPath
  #define OutputDirPath "..\..\dist"
#endif

[Setup]
AppId={{4260E570-ED2F-4140-B281-9787E93ED2E1}
AppName=WeType Clipboard Sync
AppVersion={#AppVersion}
AppPublisher=CoderDoubleflower
AppPublisherURL=https://github.com/CoderDoubleflower/wetype-clipboard-sync
AppSupportURL=https://github.com/CoderDoubleflower/wetype-clipboard-sync
DefaultDirName={localappdata}\Programs\WeTypeClipboardSync
DefaultGroupName=WeType Clipboard Sync
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x86compatible
MinVersion=10.0
OutputDir={#OutputDirPath}
OutputBaseFilename=wetype-clipboard-sync-{#AppVersion}-windows-x86-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\wxclipd.exe
LicenseFile={#PackageDir}\LICENSE
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Start clipboard sync when I sign in"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "Create a desktop shortcut for setup and pairing"; Flags: unchecked

[Files]
Source: "{#PackageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Setup and Pair"; Filename: "{app}\wxclipd.exe"; Parameters: "setup"; WorkingDir: "{app}"
Name: "{group}\Start Sync"; Filename: "{app}\wxclipd-background.exe"; WorkingDir: "{app}"
Name: "{group}\Stop Sync"; Filename: "{app}\wxclipd.exe"; Parameters: "stop"; WorkingDir: "{app}"
Name: "{group}\Sync Now"; Filename: "{app}\wxclipd.exe"; Parameters: "sync-now"; WorkingDir: "{app}"
Name: "{group}\Windows Guide"; Filename: "{sys}\notepad.exe"; Parameters: """{app}\README-Windows.md"""
Name: "{group}\Uninstall"; Filename: "{uninstallexe}"
Name: "{autodesktop}\WeType Clipboard Setup"; Filename: "{app}\wxclipd.exe"; Parameters: "setup"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "WeTypeClipboardSync"; ValueData: """{app}\wxclipd-background.exe"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\wxclipd.exe"; Parameters: "setup"; Description: "Configure proxy and pair with my phone"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "{app}\wxclipd.exe"; Parameters: "stop"; Flags: runhidden waituntilterminated; RunOnceId: "StopWeTypeClipboard"
