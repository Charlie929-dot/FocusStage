param(
    [string]$Distro = "Ubuntu",
    [string]$WslRepo = "",
    [switch]$SkipRustBuild,
    [switch]$SkipTrayBuild,
    [switch]$NoLaunch,
    [switch]$SkipMcpConfig,
    [switch]$KeepWindowHost
)

$ErrorActionPreference = "Stop"
$env:WSL_UTF8 = "1"

if (-not [string]::IsNullOrWhiteSpace($WslRepo)) {
    if ($WslRepo -eq "~") {
        $targetExpr = '"$HOME"'
    } elseif ($WslRepo.StartsWith("~/")) {
        $restEsc = $WslRepo.Substring(2) -replace "'", "'\\''"
        $targetExpr = "`"`$HOME`"/`'$restEsc`'"
    } else {
        $repoEsc = $WslRepo -replace "'", "'\\''"
        $targetExpr = "'$repoEsc'"
    }
    $resolveScript = "cd $targetExpr && pwd -P"
    $resolveB64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($resolveScript))
    $resolvedWslRepo = wsl.exe -d $Distro -e bash -lc "printf %s $resolveB64 | base64 -d | bash"
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($resolvedWslRepo)) {
        throw "Failed to resolve WSL repository '$WslRepo' in distro '$Distro'."
    }
    $WslRepo = $resolvedWslRepo.Trim()
    $RepoRoot = wsl.exe -d $Distro -e wslpath -w $WslRepo
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($RepoRoot)) {
        throw "Failed to resolve Windows path for '$WslRepo' in distro '$Distro'."
    }
    $RepoRoot = $RepoRoot.Trim()
} else {
    $RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
    if ((-not $PSBoundParameters.ContainsKey('Distro')) -and ($RepoRoot -match '^\\\\(?:wsl\.localhost|wsl\$)\\([^\\]+)\\')) {
        $Distro = $Matches[1]
    }
    $WslRepo = wsl.exe -d $Distro -e wslpath -a ($RepoRoot -replace '\\', '/')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($WslRepo)) {
        throw "Failed to resolve WSL path for $RepoRoot in distro '$Distro'."
    }
    $WslRepo = $WslRepo.Trim()
}

$IsWslUncRepo = ($RepoRoot -match '^\\\\(?:wsl\.localhost|wsl\$)\\')
$WslUid = wsl.exe -d $Distro -e /usr/bin/id -u
if ($LASTEXITCODE -ne 0 -or $WslUid -notmatch '^[0-9]+$') {
    throw "Failed to identify the Ubuntu user in '$Distro'."
}
$WslUid = $WslUid.Trim()
$InstallDir = Join-Path $env:LOCALAPPDATA "FocusStage"
if ($IsWslUncRepo) {
    $TrayBuildRoot = Join-Path ([IO.Path]::GetTempPath()) "FocusStage-tray-build"
    $TrayStageDir = Join-Path $TrayBuildRoot "tray"
    $PublishDir = Join-Path $TrayBuildRoot "publish"
} else {
    $TrayStageDir = Join-Path $RepoRoot "tray"
    $PublishDir = Join-Path $RepoRoot "tray\bin\Release\publish"
}

function Test-PathVirtualized {
    param(
        [string]$DistroName,
        [string]$DirectoryPath
    )
    New-Item -ItemType Directory -Force -Path $DirectoryPath | Out-Null
    $probeName = ".fs-vfs-probe-$([guid]::NewGuid().ToString('N'))"
    $probePath = Join-Path $DirectoryPath $probeName
    [IO.File]::WriteAllText($probePath, "probe")
    $wslProbe = wsl.exe -d $DistroName -e wslpath -a ($probePath -replace '\\', '/')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($wslProbe)) {
        Remove-Item -Force -LiteralPath $probePath -ErrorAction SilentlyContinue
        throw "Failed to resolve WSL path for virtualization probe '$probePath'."
    }
    $wslProbe = $wslProbe.Trim()
    try {
        wsl.exe -d $DistroName -e test -f $wslProbe
        return ($LASTEXITCODE -ne 0)
    } finally {
        Remove-Item -Force -LiteralPath $probePath -ErrorAction SilentlyContinue
        $wslProbeEsc = $wslProbe -replace "'", "'\\''"
        wsl.exe -d $DistroName -e bash -lc "rm -f '$wslProbeEsc'" 2>$null | Out-Null
    }
}

function Copy-DirectoryViaWsl {
    param(
        [string]$DistroName,
        [string]$SourceDir,
        [string]$DestinationDir
    )
    $wslSrc = wsl.exe -d $DistroName -e wslpath -a ($SourceDir -replace '\\', '/')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($wslSrc)) {
        throw "Failed to resolve WSL source path for '$SourceDir'."
    }
    $wslDst = wsl.exe -d $DistroName -e wslpath -a ($DestinationDir -replace '\\', '/')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($wslDst)) {
        throw "Failed to resolve WSL destination path for '$DestinationDir'."
    }
    $srcEsc = $wslSrc.Trim() -replace "'", "'\\''"
    $dstEsc = $wslDst.Trim() -replace "'", "'\\''"
    $script = "set -e`nmkdir -p '$dstEsc'`ncp -rf '$srcEsc/.' '$dstEsc/'`n"
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($script))
    wsl.exe -d $DistroName -e bash -lc "printf %s $b64 | base64 -d | bash"
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to copy '$SourceDir' to '$DestinationDir' via WSL."
    }
}

function Invoke-WslBash {
    param(
        [string]$DistroName,
        [string]$Script,
        [switch]$AsRoot
    )
    $b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes("set -e`n$Script`n"))
    $wslArgs = @("-d", $DistroName)
    if ($AsRoot) {
        $wslArgs += @("-u", "root")
    }
    $wslArgs += @("-e", "bash", "-lc", "printf %s $b64 | base64 -d | bash")
    & wsl.exe @wslArgs
    return $LASTEXITCODE
}

$isVirtualized = Test-PathVirtualized -DistroName $Distro -DirectoryPath $InstallDir
if ($isVirtualized) {
    Write-Host "Detected MSIX/VFS filesystem redirection on '$InstallDir'; deploying to real NTFS via WSL." -ForegroundColor Yellow
}

Write-Host "[1/5] Stopping existing FocusStage processes..." -ForegroundColor Cyan
if ($KeepWindowHost) {
    Get-CimInstance Win32_Process -Filter "name='FocusStage.exe'" |
        Where-Object { $_.CommandLine -notlike '*--window-host*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Get-Process -Name "FocusStageTray" -ErrorAction SilentlyContinue | Stop-Process -Force
} else {
    Invoke-WslBash -DistroName $Distro -Script "focusstage shutdown >/dev/null 2>&1 || pkill -x focusstaged >/dev/null 2>&1 || true" | Out-Null
    Get-Process -Name "FocusStage", "FocusStageTray" -ErrorAction SilentlyContinue | Stop-Process -Force
}
Start-Sleep -Milliseconds 300

if (-not $SkipRustBuild) {
    Write-Host "[2/5] Building focusstaged & focusstage (Release) in WSL '$Distro'..." -ForegroundColor Cyan
    $repoEsc = $WslRepo -replace "'", "'\\''"
    if ((Invoke-WslBash -DistroName $Distro -Script "cd '$repoEsc' && cargo build --workspace --release") -ne 0) {
        throw "cargo build --workspace --release failed."
    }
} else {
    Write-Host "[2/5] Skipping Rust build (-SkipRustBuild)." -ForegroundColor Yellow
}

Write-Host "[3/5] Installing WSL binaries and desktop helpers..." -ForegroundColor Cyan
$desktopPackages = 'python3 pkexec gnome-keyring xdg-desktop-portal xdg-desktop-portal-gtk fonts-noto-cjk xwayland xdg-utils desktop-file-utils iproute2 libcap2-bin'
$repoEsc = $WslRepo -replace "'", "'\\''"
$installScript = @(
    "install -d -m 755 /usr/local/libexec",
    "install -m 755 '$repoEsc/target/release/focusstaged' /usr/local/bin/focusstaged",
    "install -m 755 '$repoEsc/target/release/focusstage' /usr/local/bin/focusstage",
    "install -m 755 '$repoEsc/target/release/focusstage-tun' /usr/local/libexec/focusstage-tun",
    "sed 's/\r$//' '$repoEsc/scripts/focusstage-launch' > /usr/local/bin/focusstage-launch",
    "chmod 755 /usr/local/bin/focusstage-launch",
    "sed 's/\r$//' '$repoEsc/scripts/focusstage-open' > /usr/local/bin/focusstage-open",
    "chmod 755 /usr/local/bin/focusstage-open",
    "if command -v apt-get >/dev/null 2>&1 && ! dpkg -s $desktopPackages >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-upgrade $desktopPackages && fc-cache -f >/dev/null 2>&1; fi",
    "setcap cap_net_admin,cap_dac_override=ep /usr/local/libexec/focusstage-tun",
    "if test -f /usr/local/libexec/focusstage-auth; then /usr/bin/python3 -I /usr/local/libexec/focusstage-auth remove; fi",
    "/usr/local/bin/focusstage --json sync-apps --uid $WslUid >/dev/null"
) -join "`n"
if ((Invoke-WslBash -DistroName $Distro -AsRoot -Script $installScript) -ne 0) {
    throw "Failed to install WSL binaries or run sync-apps."
}
$gsettingsScript = @(
    "if command -v gsettings >/dev/null 2>&1; then",
    "  if gsettings writable org.gnome.desktop.wm.preferences button-layout 2>/dev/null | grep -qx true; then gsettings set org.gnome.desktop.wm.preferences button-layout ':minimize,maximize,close'; fi",
    "  if gsettings writable org.gnome.desktop.interface toolkit-accessibility 2>/dev/null | grep -qx true; then gsettings set org.gnome.desktop.interface toolkit-accessibility true; fi",
    "fi"
) -join "`n"
Invoke-WslBash -DistroName $Distro -Script $gsettingsScript | Out-Null

$exePath = Join-Path $InstallDir "FocusStage.exe"
$stagedExePath = $null
if (-not $SkipTrayBuild) {
    Write-Host "[4/5] Publishing Windows Launcher & Tray app to '$InstallDir'..." -ForegroundColor Cyan
    if ($IsWslUncRepo) {
        $repoTrayDir = Join-Path $RepoRoot "tray"
        New-Item -ItemType Directory -Force -Path $TrayStageDir | Out-Null
        Write-Host "Staging tray/ from WSL into native Windows build directory '$TrayStageDir'..." -ForegroundColor DarkGray
        & robocopy $repoTrayDir $TrayStageDir /MIR /XD bin obj /NFL /NDL /NJH /NJS /NC /NS /NP | Out-Null
        if ($LASTEXITCODE -ge 8) {
            throw "Failed to stage tray/ sources to '$TrayStageDir' (robocopy exit code $LASTEXITCODE)."
        }
    }
    if (Test-Path $PublishDir) {
        Remove-Item -Recurse -Force $PublishDir
    }
    New-Item -ItemType Directory -Force -Path $PublishDir | Out-Null
    $csproj = Join-Path $TrayStageDir "FocusStageTray.csproj"
    dotnet publish $csproj -c Release -o $PublishDir --nologo
    if ($LASTEXITCODE -ne 0) {
        throw "dotnet publish failed."
    }

    $stagedExePath = Join-Path $PublishDir "FocusStage.exe"
    if (-not (Test-Path $stagedExePath)) {
        throw "Expected launcher binary missing at $stagedExePath"
    }

    # Verify the newly published executable before installing and registering its path.
    $hostStart = [System.Diagnostics.ProcessStartInfo]::new($stagedExePath, "--window-host")
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
            throw "The installed FocusStage.exe did not complete the window host handshake: $stagedExePath"
        }
        $ready = $readyLine.Result | ConvertFrom-Json
        if ($ready.type -ne "ready" -or $ready.scale -le 0 -or $ready.width -le 0 -or $ready.height -le 0 -or $hostProcess.HasExited) {
            throw "The installed FocusStage.exe returned an invalid window host handshake: $stagedExePath"
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

    [IO.File]::WriteAllText((Join-Path $PublishDir "distro.txt"), $Distro + "`n")

    if ($isVirtualized) {
        Copy-DirectoryViaWsl -DistroName $Distro -SourceDir $PublishDir -DestinationDir $InstallDir
    } else {
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        Get-ChildItem -Path $InstallDir -Filter '*.old.*' -ErrorAction SilentlyContinue |
            ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
        $stamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
        foreach ($item in (Get-ChildItem -Path $PublishDir)) {
            $dst = Join-Path $InstallDir $item.Name
            if ($item.PSIsContainer) {
                Copy-Item -Path $item.FullName -Destination $InstallDir -Recurse -Force
                continue
            }
            try {
                Copy-Item -LiteralPath $item.FullName -Destination $dst -Force
            } catch {
                if (Test-Path -LiteralPath $dst) {
                    Move-Item -LiteralPath $dst -Destination "$dst.old.$stamp" -Force
                }
                Copy-Item -LiteralPath $item.FullName -Destination $dst -Force
            }
        }
    }
} else {
    Write-Host "[4/5] Skipping Windows Launcher & Tray build (-SkipTrayBuild)." -ForegroundColor Yellow
}
$wslHostPath = wsl.exe -d $Distro -e wslpath -a ($exePath -replace '\\', '/')
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($wslHostPath)) {
    throw "Failed to resolve the Windows host path for WSL '$Distro'."
}
$wslHostPath = $wslHostPath.TrimEnd("`r", "`n")
wsl.exe -d $Distro -e test -f $wslHostPath
if ($LASTEXITCODE -ne 0) {
    throw "Deployed FocusStage.exe was not found on the real filesystem at '$wslHostPath'. Run without -SkipTrayBuild first."
}
$hostPathBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($wslHostPath))
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
$shortcutRunner = if ($stagedExePath) { $stagedExePath } elseif (-not $isVirtualized) { $exePath } else { $null }
if ($shortcutRunner) {
    $syncProc = Start-Process -FilePath $shortcutRunner -ArgumentList @("--sync-shortcuts", "--distro", $Distro) -Wait -PassThru
    if ($syncProc.ExitCode -ne 0) {
        throw "FocusStage --sync-shortcuts failed (exit code $($syncProc.ExitCode))."
    }
    if ($isVirtualized -and $stagedExePath) {
        $virtualIconsDir = Join-Path $InstallDir "icons"
        if (Test-Path $virtualIconsDir) {
            $stagedIconsDir = Join-Path $PublishDir "_icons"
            New-Item -ItemType Directory -Force -Path $stagedIconsDir | Out-Null
            try {
                Copy-Item -Path (Join-Path $virtualIconsDir "*") -Destination $stagedIconsDir -Recurse -Force
                Copy-DirectoryViaWsl -DistroName $Distro -SourceDir $stagedIconsDir -DestinationDir $virtualIconsDir
            } finally {
                Remove-Item -Recurse -Force -LiteralPath $stagedIconsDir -ErrorAction SilentlyContinue
            }
        }
    }
} else {
    Write-Host "Skipping shortcut icon sync with -SkipTrayBuild inside MSIX/virtualized host." -ForegroundColor Yellow
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
    if ($isVirtualized) {
        Write-Host ""
        Write-Host "Skipping auto-launch inside MSIX/virtualized host so FocusStage does not run against LocalCache." -ForegroundColor Yellow
        Write-Host "Launch FocusStage from the Desktop or Start Menu shortcut." -ForegroundColor Yellow
    } else {
        Start-Process $exePath -ArgumentList "--distro `"$Distro`""
    }
}
