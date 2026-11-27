param(
    [string]$Distro = "",
    [switch]$KeepInstallDir
)

$ErrorActionPreference = "Continue"
$env:WSL_UTF8 = "1"

$installDir = Join-Path $env:LOCALAPPDATA "FocusStage"
if ([string]::IsNullOrWhiteSpace($Distro)) {
    $distroCandidates = @(
        (Join-Path $PSScriptRoot "distro.txt"),
        (Join-Path $installDir "distro.txt")
    )
    foreach ($candidate in $distroCandidates) {
        if (Test-Path -LiteralPath $candidate) {
            $saved = (Get-Content -LiteralPath $candidate -Raw -ErrorAction SilentlyContinue).Trim()
            if (-not [string]::IsNullOrWhiteSpace($saved)) {
                $Distro = $saved
                break
            }
        }
    }
    if ([string]::IsNullOrWhiteSpace($Distro)) {
        $Distro = "Ubuntu"
    }
}

Write-Host "Stopping FocusStage processes..." -ForegroundColor Cyan
Get-Process -Name "FocusStage", "FocusStageTray" -ErrorAction SilentlyContinue | Stop-Process -Force
$unregisterHost = 'focusstage shutdown >/dev/null 2>&1 || pkill -x focusstaged >/dev/null 2>&1 || true; focusstage settings remove-handlers >/dev/null 2>&1 || true; rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/focusstage/native-host-path"'
$unregisterHostBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($unregisterHost))
wsl.exe -d $Distro -e bash -lc "printf %s $unregisterHostBase64 | base64 -d | bash"

Write-Host "Removing FocusStage session authorization..." -ForegroundColor Cyan
wsl.exe -d $Distro -u root -e sh -c 'if test -f /usr/local/libexec/focusstage-auth; then /usr/bin/python3 -I /usr/local/libexec/focusstage-auth remove; fi'
if ($LASTEXITCODE -ne 0) {
    throw "Failed to remove FocusStage session authorization from '$Distro'."
}

Write-Host "Removing FocusStage desktop wrappers..." -ForegroundColor Cyan
wsl.exe -d $Distro -u root -e bash -lc "/usr/local/bin/focusstage sync-apps --remove >/dev/null 2>&1 || true; rm -f /usr/local/bin/focusstaged /usr/local/bin/focusstage /usr/local/libexec/focusstage-tun /usr/local/bin/focusstage-launch /usr/local/bin/focusstage-open /etc/polkit-1/rules.d/49-focusstage-gvfs-admin.rules"

Write-Host "Removing FocusStage from Claude Code and Codex MCP configurations..." -ForegroundColor Cyan
$wslWinHome = wsl.exe -d $Distro -e wslpath -a ($HOME -replace '\\', '/')
$mcpHelper = Join-Path $PSScriptRoot "mcp-config.py"
if (-not (Test-Path -LiteralPath $mcpHelper)) {
    $mcpHelper = Join-Path $installDir "mcp-config.py"
}
$wslHelper = if (Test-Path -LiteralPath $mcpHelper) {
    wsl.exe -d $Distro -e wslpath -a ($mcpHelper -replace '\\', '/')
} else {
    ""
}
if ([string]::IsNullOrWhiteSpace($wslWinHome) -or [string]::IsNullOrWhiteSpace($wslHelper)) {
    Write-Warning "Failed to resolve the MCP configuration or helper path in WSL."
} else {
    wsl.exe -d $Distro -e python3 -I $wslHelper.Trim() --windows-home $wslWinHome.Trim() --remove
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "The FocusStage MCP entries could not be fully removed."
    }
}

Write-Host "Removing Windows shortcuts and installed files..." -ForegroundColor Cyan
$startDir = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs\FocusStage"
$startTop = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs\FocusStage.lnk"
$desktopLnk = Join-Path ([Environment]::GetFolderPath("DesktopDirectory")) "FocusStage.lnk"
$startupLnk = Join-Path ([Environment]::GetFolderPath("Startup")) "FocusStage.lnk"

Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $startDir
if (-not $KeepInstallDir) {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $installDir
}
Remove-Item -Force -ErrorAction SilentlyContinue $startTop, $desktopLnk, $startupLnk

Write-Host "FocusStage uninstalled." -ForegroundColor Green
