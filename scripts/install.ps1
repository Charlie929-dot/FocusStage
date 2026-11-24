param(
    [string]$Distro = "Ubuntu",
    [switch]$SkipRustBuild,
    [switch]$NoLaunch,
    [switch]$SkipMcpConfig
)

$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$WslRepo = wsl.exe -d $Distro -e wslpath -a ($RepoRoot -replace '\\', '/')
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($WslRepo)) {
    throw "Failed to resolve WSL path for $RepoRoot in distro '$Distro'."
}
$WslRepo = $WslRepo.Trim()
$WslUid = wsl.exe -d $Distro -e /usr/bin/id -u
if ($LASTEXITCODE -ne 0 -or $WslUid -notmatch '^[0-9]+$') {
    throw "Failed to identify the Ubuntu user in '$Distro'."
}
$WslUid = $WslUid.Trim()
$InstallDir = Join-Path $env:LOCALAPPDATA "FocusStage"

Write-Host "[1/5] Stopping existing FocusStage processes..." -ForegroundColor Cyan
Get-Process -Name "FocusStage", "FocusStageTray" -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
wsl.exe -d $Distro -e bash -lc "focusstage shutdown >/dev/null 2>&1 || pkill -x focusstaged >/dev/null 2>&1 || true"

if (-not $SkipRustBuild) {
    Write-Host "[2/5] Building focusstaged & focusstage (Release) in WSL '$Distro'..." -ForegroundColor Cyan
    wsl.exe -d $Distro -e bash -lc "cd '$WslRepo' && cargo build --workspace --release"
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --workspace --release failed."
    }
} else {
    Write-Host "[2/5] Skipping Rust build (-SkipRustBuild)." -ForegroundColor Yellow
}

Write-Host "[3/5] Installing WSL binaries and desktop helpers..." -ForegroundColor Cyan
$desktopPackages = 'python3 pkexec policykit-1-gnome gnome-keyring xdg-desktop-portal xdg-desktop-portal-gtk fonts-noto-cjk xwayland xdg-utils desktop-file-utils'
$installCmd = @(
    "install -m 755 '$WslRepo/target/release/focusstaged' /usr/local/bin/focusstaged",
    "install -m 755 '$WslRepo/target/release/focusstage' /usr/local/bin/focusstage",
    "sed 's/\r`$//' '$WslRepo/scripts/focusstage-launch' > /usr/local/bin/focusstage-launch",
    "chmod 755 /usr/local/bin/focusstage-launch",
    "sed 's/\r`$//' '$WslRepo/scripts/focusstage-open' > /usr/local/bin/focusstage-open",
    "chmod 755 /usr/local/bin/focusstage-open",
    "if command -v apt-get >/dev/null 2>&1 && ! dpkg -s $desktopPackages >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $desktopPackages && fc-cache -f >/dev/null 2>&1; fi",
    "/usr/bin/python3 -I '$WslRepo/scripts/focusstage-auth' setup $WslUid",
    "/usr/local/bin/focusstage --json sync-apps --uid $WslUid >/dev/null"
) -join " && "
wsl.exe -d $Distro -u root -e bash -lc $installCmd
if ($LASTEXITCODE -ne 0) {
    throw "Failed to install WSL binaries or run sync-apps."
}
wsl.exe -d $Distro -e bash -lc "if command -v gsettings >/dev/null 2>&1; then if gsettings writable org.gnome.desktop.wm.preferences button-layout 2>/dev/null | grep -qx true; then gsettings set org.gnome.desktop.wm.preferences button-layout ':minimize,maximize,close'; fi; if gsettings writable org.gnome.desktop.interface toolkit-accessibility 2>/dev/null | grep -qx true; then gsettings set org.gnome.desktop.interface toolkit-accessibility true; fi; fi" | Out-Null

Write-Host "[4/5] Publishing Windows Launcher & Tray app to '$InstallDir'..." -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$csproj = Join-Path $RepoRoot "tray\FocusStageTray.csproj"
dotnet publish $csproj -c Release -o $InstallDir --nologo
if ($LASTEXITCODE -ne 0) {
    throw "dotnet publish failed."
}

$exePath = Join-Path $InstallDir "FocusStage.exe"
if (-not (Test-Path $exePath)) {
    throw "Expected launcher binary missing at $exePath"
}

# Verify the executable that Windows will actually run before registering its path.
$hostStart = [System.Diagnostics.ProcessStartInfo]::new($exePath, "--window-host")
$hostStart.UseShellExecute = $false
$hostStart.RedirectStandardInput = $true
$hostStart.RedirectStandardOutput = $true
$previousInputEncoding = [Console]::InputEncoding
try {
    # Windows PowerShell uses the console encoding for the child stdin pipe.
    [Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
    $hostProcess = [System.Diagnostics.Process]::Start($hostStart)
} finally {
    [Console]::InputEncoding = $previousInputEncoding
}
try {
    $readyLine = $hostProcess.StandardOutput.ReadLineAsync()
    if (-not $readyLine.Wait(5000) -or [string]::IsNullOrWhiteSpace($readyLine.Result)) {
        throw "The installed FocusStage.exe did not complete the window host handshake: $exePath"
    }
    $ready = $readyLine.Result | ConvertFrom-Json
    if ($ready.type -ne "ready" -or $ready.scale -le 0 -or $ready.width -le 0 -or $ready.height -le 0 -or $hostProcess.HasExited) {
        throw "The installed FocusStage.exe returned an invalid window host handshake: $exePath"
    }
} finally {
    if (-not $hostProcess.HasExited) {
        $hostProcess.StandardInput.BaseStream.Close()
        if (-not $hostProcess.WaitForExit(2000)) {
            $hostProcess.Kill()
            $hostProcess.WaitForExit()
        }
    }
    $hostProcess.Dispose()
}

$wslHostPath = wsl.exe -d $Distro -e wslpath -a ($exePath -replace '\\', '/')
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($wslHostPath)) {
    throw "Failed to resolve the Windows host path for WSL '$Distro'."
}
$hostPathBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($wslHostPath.TrimEnd("`r", "`n")))
$registerHost = 'dir="${XDG_CONFIG_HOME:-$HOME/.config}/focusstage"; umask 077; mkdir -p "$dir" && printf %s ' + $hostPathBase64 + ' | base64 -d > "$dir/native-host-path"'
# Windows PowerShell can strip embedded quotes from arguments to wsl.exe.
$registerHostBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($registerHost))
wsl.exe -d $Distro -e bash -lc "printf %s $registerHostBase64 | base64 -d | bash"
if ($LASTEXITCODE -ne 0) {
    throw "Failed to register the native Windows host in WSL '$Distro'."
}

Write-Host "[5/5] Initializing Linux protocol associations and creating Windows shortcuts..." -ForegroundColor Cyan
wsl.exe -d $Distro -e /usr/local/bin/focusstage --json settings init-defaults | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Failed to initialize FocusStage protocol associations."
}
& $exePath --sync-shortcuts --distro $Distro
if ($LASTEXITCODE -ne 0) {
    throw "FocusStage --sync-shortcuts failed."
}

if (-not $SkipMcpConfig) {
    Write-Host "Registering FocusStage with Claude Code MCP..." -ForegroundColor Cyan
    $claudeConfig = Join-Path $HOME ".claude.json"
    $wslConfig = wsl.exe -d $Distro -e wslpath -a ($claudeConfig -replace '\\', '/')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($wslConfig)) {
        throw "Failed to resolve the Claude Code configuration path in WSL."
    }
    wsl.exe -d $Distro -e python3 -I "$WslRepo/scripts/claude-mcp.py" $wslConfig.Trim() --distro $Distro
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to register the Claude Code MCP server."
    }
}

Write-Host ""
Write-Host "FocusStage installed successfully!" -ForegroundColor Green
Write-Host "  - Windows Launcher : $exePath"
Write-Host "  - Desktop Shortcut : $(Join-Path ([Environment]::GetFolderPath('DesktopDirectory')) 'FocusStage.lnk')"
Write-Host "  - Start Menu Folder: $(Join-Path ([Environment]::GetFolderPath('StartMenu')) 'Programs\FocusStage')"

if (-not $NoLaunch) {
    Start-Process $exePath -ArgumentList "--distro `"$Distro`""
}
