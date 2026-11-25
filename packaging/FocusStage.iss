#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef ReleaseTag
  #define ReleaseTag "v0.1.0"
#endif
#ifndef PayloadDir
  #define PayloadDir "..\dist\payload"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

[Setup]
AppId={{7E8B63E3-9F2C-4F6D-B62E-8B1F9B43D60A}
AppName=FocusStage
AppVersion={#AppVersion}
AppVerName=FocusStage {#ReleaseTag}
AppPublisher=Charlie929-dot
AppPublisherURL=https://github.com/Charlie929-dot/FocusStage
AppSupportURL=https://github.com/Charlie929-dot/FocusStage/issues
DefaultDirName={localappdata}\FocusStage
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=FocusStage-Setup-{#ReleaseTag}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName=FocusStage
UninstallDisplayIcon={app}\FocusStage.exe

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{tmp}\payload"; Flags: ignoreversion recursesubdirs createallsubdirs

[Run]
Filename: "powershell.exe"; Parameters: "{code:GetInstallerArgs}"; StatusMsg: "Installing FocusStage into WSL2 Ubuntu and Windows..."; Flags: waituntilterminated

[UninstallRun]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\uninstall.ps1"" -KeepInstallDir"; Flags: waituntilterminated; RunOnceId: "FocusStageUninstall"

[UninstallDelete]
Type: filesandordirs; Name: "{app}"

[Code]
function GetInstallerArgs(Param: String): String;
begin
  Result := '-NoProfile -ExecutionPolicy Bypass -File "' + ExpandConstant('{tmp}\payload\install-release.ps1') + '"';
  if WizardSilent then
    Result := Result + ' -NonInteractive -NoLaunch'
  else
    Result := Result + ' -PauseOnError';
end;
