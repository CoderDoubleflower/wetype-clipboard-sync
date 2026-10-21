#ifndef AppVersion
  #define AppVersion "0.4.2"
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
SetupIconFile={#PackageDir}\wetype.ico
UninstallDisplayIcon={app}\wxclipd-background.exe
LicenseFile={#PackageDir}\LICENSE
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Start clipboard sync when I sign in"; GroupDescription: "Startup:"
Name: "desktopicon"; Description: "Create a desktop shortcut for status and settings"; Flags: unchecked

[InstallDelete]
Type: files; Name: "{autodesktop}\WeType Clipboard Setup.lnk"

[Files]
Source: "{#PackageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\WeType Clipboard Sync"; Filename: "{app}\wxclipd-background.exe"; Parameters: "--show"; WorkingDir: "{app}"
Name: "{group}\Setup and Pair"; Filename: "{app}\wxclipd.exe"; Parameters: "setup"; WorkingDir: "{app}"
Name: "{group}\Start Sync"; Filename: "{app}\wxclipd-background.exe"; WorkingDir: "{app}"
Name: "{group}\Stop Sync"; Filename: "{app}\wxclipd.exe"; Parameters: "stop"; WorkingDir: "{app}"
Name: "{group}\Sync Now"; Filename: "{app}\wxclipd.exe"; Parameters: "sync-now"; WorkingDir: "{app}"
Name: "{group}\Windows Guide"; Filename: "{sys}\notepad.exe"; Parameters: """{app}\README-Windows.md"""; IconFilename: "{app}\wxclipd-background.exe"
Name: "{group}\Uninstall"; Filename: "{uninstallexe}"
Name: "{autodesktop}\WeType Clipboard Sync"; Filename: "{app}\wxclipd-background.exe"; Parameters: "--show"; Tasks: desktopicon; WorkingDir: "{app}"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "WeTypeClipboardSync"; ValueData: """{app}\wxclipd-background.exe"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\wxclipd-background.exe"; Parameters: "--show"; Description: "Open clipboard sync status and settings"; Flags: postinstall nowait skipifsilent

[Code]
function StopRunningApp(): String;
var
  CliPath, TrayPath: String;
  ExitCode: Integer;
  VersionMS, VersionLS: Cardinal;
begin
  Result := '';
  CliPath := ExpandConstant('{app}\wxclipd.exe');
  TrayPath := ExpandConstant('{app}\wxclipd-background.exe');
  { Version resources were introduced together with the resident tray in 0.4. }
  if FileExists(TrayPath) and GetVersionNumbers(TrayPath, VersionMS, VersionLS) then
  begin
    if VersionMS >= $00000004 then
    begin
      if not Exec(TrayPath, '--quit', '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then
        Result := 'Unable to close the clipboard tray. Exit from its menu and try again.'
      else if ExitCode <> 0 then
        Result := 'The clipboard tray could not close cleanly. Exit from its menu and try again.';
      Exit;
    end;
  end;
  { The 0.2/0.3 launcher does not understand --quit and would display a blocking
    error box. Stop its worker through the backwards-compatible CLI instead. }
  if FileExists(CliPath) then
  begin
    if not Exec(CliPath, 'stop', '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then
      Result := 'Unable to stop clipboard sync. Close it and try again.'
    else if ExitCode <> 0 then
      Result := 'Clipboard sync could not stop cleanly. Close it and try again.';
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := StopRunningApp();
end;

function InitializeUninstall(): Boolean;
var
  Error: String;
begin
  Error := StopRunningApp();
  Result := Error = '';
  if not Result then SuppressibleMsgBox(Error, mbError, MB_OK, IDOK);
end;
