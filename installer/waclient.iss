; Per-user installer for WaClient. Compile via scripts\package.ps1 -Installer, or manually:
;   ISCC /DAppVersion=0.1.0 /DSourceExe=..\target\release\waclient.exe /DOutDir=..\dist installer\waclient.iss
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\waclient.exe"
#endif
#ifndef OutDir
  #define OutDir "..\dist"
#endif
#define AppId "WaClient.Desktop"

[Setup]
AppId={{B6B5A0C2-5C1E-4B7A-9D55-3A1F6C0E7A21}
AppName=WaClient
AppVersion={#AppVersion}
AppPublisher=WaClient
DefaultDirName={localappdata}\Programs\WaClient
DefaultGroupName=WaClient
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile=..\assets\waclient.ico
UninstallDisplayIcon={app}\WaClient.exe
OutputDir={#OutDir}
OutputBaseFilename=WaClient-{#AppVersion}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked
Name: "autostart"; Description: "Start WaClient when I sign in"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "WaClient.exe"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The AppUserModelID must match the one the app sets at runtime so toasts are attributed to WaClient.
Name: "{autoprograms}\WaClient"; Filename: "{app}\WaClient.exe"; AppUserModelID: "{#AppId}"
Name: "{autodesktop}\WaClient"; Filename: "{app}\WaClient.exe"; AppUserModelID: "{#AppId}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "WaClient"; ValueData: """{app}\WaClient.exe"" --minimized"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\WaClient.exe"; Description: "Launch WaClient"; Flags: nowait postinstall skipifsilent

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
begin
  if CurUninstallStep = usUninstall then
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'WaClient');
  if CurUninstallStep = usPostUninstall then
  begin
    DataDir := ExpandConstant('{userappdata}\WaClient');
    if DirExists(DataDir) then
      if MsgBox('Also delete your WaClient data (messages, login session, settings) in:' + #13#10 + DataDir + '?' + #13#10#13#10 +
                'Choose No to keep it.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(DataDir, True, True, True);
  end;
end;
