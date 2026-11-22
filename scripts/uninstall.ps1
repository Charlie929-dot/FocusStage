param(
    [string]$Distro = "Ubuntu"
)

$ErrorActionPreference = "Continue"

Write-Host "Stopping FocusStage processes..." -ForegroundColor Cyan
Get-Process -Name "FocusStage", "FocusStageTray" -ErrorAction SilentlyContinue | Stop-Process -Force
$unregisterHost = 'focusstage shutdown >/dev/null 2>&1 || pkill -x focusstaged >/dev/null 2>&1 || true; rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/focusstage/native-host-path"'
$unregisterHostBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($unregisterHost))
wsl.exe -d $Distro -e bash -lc "printf %s $unregisterHostBase64 | base64 -d | bash"

Write-Host "Removing WSLg FocusStage desktop wrappers..." -ForegroundColor Cyan
wsl.exe -d $Distro -u root -e bash -lc "/usr/local/bin/focusstage sync-apps --remove >/dev/null 2>&1 || true; rm -f /usr/local/bin/focusstaged /usr/local/bin/focusstage /usr/local/bin/focusstage-launch"

Write-Host "Removing Windows shortcuts and installed files..." -ForegroundColor Cyan
$installDir = Join-Path $env:LOCALAPPDATA "FocusStage"
$startDir = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs\FocusStage"
$startTop = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs\FocusStage.lnk"
$desktopLnk = Join-Path ([Environment]::GetFolderPath("DesktopDirectory")) "FocusStage.lnk"
$startupLnk = Join-Path ([Environment]::GetFolderPath("Startup")) "FocusStage.lnk"

Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $startDir, $installDir
Remove-Item -Force -ErrorAction SilentlyContinue $startTop, $desktopLnk, $startupLnk

Write-Host "FocusStage uninstalled." -ForegroundColor Green
