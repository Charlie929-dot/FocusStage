param(
    [string]$Distro = "",
    [switch]$NoLaunch,
    [switch]$SkipMcpConfig,
    [switch]$NonInteractive,
    [switch]$PauseOnError
)

$ErrorActionPreference = "Stop"
$env:WSL_UTF8 = "1"

function Fail-Install {
    param([string]$Message)
    Write-Host ""
    Write-Host "ERROR: $Message" -ForegroundColor Red
    if ($PauseOnError -and (-not $NonInteractive)) {
        Write-Host ""
        Read-Host "Press Enter to close"
    }
    exit 1
}

try {
    $PayloadRoot = $PSScriptRoot
    $WinPayloadDir = Join-Path $PayloadRoot "win"
    $LinuxPayloadDir = Join-Path $PayloadRoot "linux"
    $ScriptsPayloadDir = Join-Path $PayloadRoot "scripts"

    $requiredFiles = @(
        (Join-Path $WinPayloadDir "FocusStage.exe"),
        (Join-Path $LinuxPayloadDir "focusstaged"),
        (Join-Path $LinuxPayloadDir "focusstage"),
        (Join-Path $LinuxPayloadDir "focusstage-tun"),
        (Join-Path $ScriptsPayloadDir "focusstage-launch"),
        (Join-Path $ScriptsPayloadDir "focusstage-open"),
        (Join-Path $ScriptsPayloadDir "mcp-config.py"),
        (Join-Path $ScriptsPayloadDir "uninstall.ps1")
    )
    foreach ($file in $requiredFiles) {
        if (-not (Test-Path -LiteralPath $file)) {
            throw "Release payload is incomplete. Missing: $file"
        }
    }

    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
        throw "wsl.exe was not found. FocusStage requires Windows 10/11 with WSL2 enabled."
    }

    function Get-CandidateDistros {
        $ignore = @("docker-desktop", "docker-desktop-data", "rancher-desktop", "podman-machine-default")
        $raw = & wsl.exe -l -q 2>$null
        if ($LASTEXITCODE -ne 0) {
            return $null
        }
        $list = @()
        foreach ($line in $raw) {
            $name = ($line -replace "`0", "").Trim()
            if (-not [string]::IsNullOrWhiteSpace($name) -and ($ignore -notcontains $name)) {
                $list += $name
            }
        }
        return ,$list
    }

    function Test-IsUbuntuDistro {
        param([string]$DistroName)
        $osId = & wsl.exe -d $DistroName -e sh -c '. /etc/os-release 2>/dev/null && printf %s "$ID"' 2>$null
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($osId)) {
            return $null
        }
        return $osId.Trim()
    }

    $distros = Get-CandidateDistros
    if ($null -eq $distros) {
        if (-not $NonInteractive) {
            Write-Host "WSL2 does not appear to be initialized on this PC yet." -ForegroundColor Yellow
            $ans = Read-Host "Enable WSL2 now via 'wsl --install --no-distribution' (requires Admin & likely a reboot)? [Y/n]"
            if ([string]::IsNullOrWhiteSpace($ans) -or $ans -match '^[yY]') {
                Start-Process -FilePath "wsl.exe" -ArgumentList "--install --no-distribution" -Verb RunAs -Wait
                Write-Host ""
                Write-Host "WSL2 setup was triggered. If Windows asks for a reboot, restart your PC and run this installer again." -ForegroundColor Yellow
            }
        }
        throw "WSL2 is not ready yet. Ensure WSL2 is enabled and rerun the FocusStage installer."
    }

    if ([string]::IsNullOrWhiteSpace($Distro)) {
        $ubuntuDistros = @()
        foreach ($cand in $distros) {
            $osId = Test-IsUbuntuDistro -DistroName $cand
            if ($osId -eq "ubuntu") {
                $ubuntuDistros += $cand
            }
        }

        if ($ubuntuDistros.Count -eq 0) {
            if (-not $NonInteractive) {
                Write-Host "No Ubuntu WSL distribution was found." -ForegroundColor Yellow
                $ans = Read-Host "Install Ubuntu-24.04 now via 'wsl --install -d Ubuntu-24.04'? [Y/n]"
                if ([string]::IsNullOrWhiteSpace($ans) -or $ans -match '^[yY]') {
                    Write-Host "Installing Ubuntu-24.04 in WSL2 (complete the Linux username/password prompt if shown)..." -ForegroundColor Cyan
                    & wsl.exe --install -d Ubuntu-24.04
                    $distros = Get-CandidateDistros
                    if ($null -ne $distros) {
                        foreach ($cand in $distros) {
                            if ((Test-IsUbuntuDistro -DistroName $cand) -eq "ubuntu") {
                                $ubuntuDistros += $cand
                            }
                        }
                    }
                }
            }
            if ($ubuntuDistros.Count -eq 0) {
                throw "No Ubuntu WSL distribution found. Install one with 'wsl --install -d Ubuntu-24.04' and rerun the installer."
            }
        }

        if ($ubuntuDistros.Count -eq 1) {
            $Distro = $ubuntuDistros[0]
        } elseif ($NonInteractive) {
            $Distro = if ($ubuntuDistros -contains "Ubuntu") { "Ubuntu" } else { $ubuntuDistros[0] }
        } else {
            Write-Host "Multiple Ubuntu WSL distributions found:" -ForegroundColor Cyan
            for ($i = 0; $i -lt $ubuntuDistros.Count; $i++) {
                Write-Host ("  [{0}] {1}" -f ($i + 1), $ubuntuDistros[$i])
            }
            $defaultIdx = $ubuntuDistros.IndexOf("Ubuntu")
            if ($defaultIdx -lt 0) { $defaultIdx = 0 }
            $choice = Read-Host ("Select target distro [default {0}]" -f ($defaultIdx + 1))
            if ([string]::IsNullOrWhiteSpace($choice)) {
                $Distro = $ubuntuDistros[$defaultIdx]
            } elseif (($choice -match '^[0-9]+$') -and ([int]$choice -ge 1) -and ([int]$choice -le $ubuntuDistros.Count)) {
                $Distro = $ubuntuDistros[[int]$choice - 1]
            } else {
                throw "Invalid distro selection '$choice'."
            }
        }
    } else {
        $osId = Test-IsUbuntuDistro -DistroName $Distro
        if ($null -eq $osId) {
            throw "WSL distribution '$Distro' is not installed or failed to start."
        }
        if ($osId -ne "ubuntu") {
            throw "WSL distribution '$Distro' reports ID='$osId'. This installer currently supports Ubuntu WSL distributions only."
        }
    }

    Write-Host "Target WSL distribution: $Distro" -ForegroundColor Green

    $WslUid = wsl.exe -d $Distro -e /usr/bin/id -u
    if ($LASTEXITCODE -ne 0 -or $WslUid -notmatch '^[0-9]+$') {
        throw "Failed to identify the Ubuntu user in '$Distro'."
    }
    $WslUid = $WslUid.Trim()

    $InstallDir = Join-Path $env:LOCALAPPDATA "FocusStage"

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

    function Convert-ToWslPath {
        param(
            [string]$DistroName,
            [string]$WinPath
        )
        $resolved = wsl.exe -d $DistroName -e wslpath -a ($WinPath -replace '\\', '/')
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($resolved)) {
            throw "Failed to resolve WSL path for '$WinPath'."
        }
        return $resolved.Trim()
    }

    $isVirtualized = Test-PathVirtualized -DistroName $Distro -DirectoryPath $InstallDir
    if ($isVirtualized) {
        Write-Host "Detected MSIX/VFS filesystem redirection on '$InstallDir'; deploying to real NTFS via WSL." -ForegroundColor Yellow
    }

    Write-Host "[1/5] Stopping existing FocusStage processes..." -ForegroundColor Cyan
    wsl.exe -d $Distro -e bash -lc "focusstage shutdown >/dev/null 2>&1 || pkill -x focusstaged >/dev/null 2>&1 || true"
    Get-Process -Name "FocusStage", "FocusStageTray" -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 300

    $WslLinuxPayload = Convert-ToWslPath -DistroName $Distro -WinPath $LinuxPayloadDir
    $WslScriptsPayload = Convert-ToWslPath -DistroName $Distro -WinPath $ScriptsPayloadDir

    Write-Host "[2/5] Checking and installing Ubuntu runtime packages (missing only)..." -ForegroundColor Cyan
    $runtimePackages = 'python3 pkexec gnome-keyring xdg-desktop-portal xdg-desktop-portal-gtk fonts-noto-cjk xwayland xdg-utils desktop-file-utils libpixman-1-0 libwayland-server0 libxkbcommon0 libffi8 iproute2 libcap2-bin'
    $aptCmd = "if ! dpkg -s $runtimePackages >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-upgrade $runtimePackages && fc-cache -f >/dev/null 2>&1; fi"
    $aptB64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes("set -e`n$aptCmd`n"))
    wsl.exe -d $Distro -u root -e bash -lc "printf %s $aptB64 | base64 -d | bash"
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to install Ubuntu runtime packages via apt-get. Check your network connection and apt sources in '$Distro'."
    }

    Write-Host "[3/5] Installing FocusStage WSL binaries and desktop helpers..." -ForegroundColor Cyan
    $linuxEsc = $WslLinuxPayload -replace "'", "'\\''"
    $scriptsEsc = $WslScriptsPayload -replace "'", "'\\''"
    $installScript = @(
        "set -e",
        "install -d -m 755 /usr/local/libexec",
        "install -m 755 '$linuxEsc/focusstaged' /usr/local/bin/focusstaged",
        "install -m 755 '$linuxEsc/focusstage' /usr/local/bin/focusstage",
        "install -m 755 '$linuxEsc/focusstage-tun' /usr/local/libexec/focusstage-tun",
        "setcap cap_net_admin,cap_dac_override=ep /usr/local/libexec/focusstage-tun",
        "sed 's/\r`$//' '$scriptsEsc/focusstage-launch' > /usr/local/bin/focusstage-launch",
        "chmod 755 /usr/local/bin/focusstage-launch",
        "sed 's/\r`$//' '$scriptsEsc/focusstage-open' > /usr/local/bin/focusstage-open",
        "chmod 755 /usr/local/bin/focusstage-open",
        "if test -f '$scriptsEsc/49-focusstage-gvfs-admin.rules'; then install -d -m 755 /etc/polkit-1/rules.d && sed 's/\r`$//' '$scriptsEsc/49-focusstage-gvfs-admin.rules' > /etc/polkit-1/rules.d/49-focusstage-gvfs-admin.rules && chmod 644 /etc/polkit-1/rules.d/49-focusstage-gvfs-admin.rules; fi",
        "if test -f /usr/local/libexec/focusstage-auth; then /usr/bin/python3 -I /usr/local/libexec/focusstage-auth remove; fi",
        "/usr/local/bin/focusstage --json sync-apps --uid $WslUid >/dev/null"
    ) -join "`n"
    $installB64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($installScript + "`n"))
    wsl.exe -d $Distro -u root -e bash -lc "printf %s $installB64 | base64 -d | bash"
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to install WSL binaries or run sync-apps."
    }
    wsl.exe -d $Distro -e bash -lc "if command -v gsettings >/dev/null 2>&1; then if gsettings writable org.gnome.desktop.wm.preferences button-layout 2>/dev/null | grep -qx true; then gsettings set org.gnome.desktop.wm.preferences button-layout ':minimize,maximize,close'; fi; if gsettings writable org.gnome.desktop.interface toolkit-accessibility 2>/dev/null | grep -qx true; then gsettings set org.gnome.desktop.interface toolkit-accessibility true; fi; fi" | Out-Null

    Write-Host "[4/5] Verifying and deploying Windows Launcher & Tray app to '$InstallDir'..." -ForegroundColor Cyan
    Get-ChildItem -Path $WinPayloadDir -Recurse -File | Unblock-File -ErrorAction SilentlyContinue
    $stagedExePath = Join-Path $WinPayloadDir "FocusStage.exe"
    $hostStart = [System.Diagnostics.ProcessStartInfo]::new($stagedExePath, "--window-host")
    $hostStart.UseShellExecute = $false
    $hostStart.RedirectStandardInput = $true
    $hostStart.RedirectStandardOutput = $true
    $previousInputEncoding = [Console]::InputEncoding
    try {
        [Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
        $hostProcess = [System.Diagnostics.Process]::Start($hostStart)
    } finally {
        [Console]::InputEncoding = $previousInputEncoding
    }
    try {
        $readyLine = $hostProcess.StandardOutput.ReadLineAsync()
        if (-not $readyLine.Wait(5000) -or [string]::IsNullOrWhiteSpace($readyLine.Result)) {
            throw "FocusStage.exe did not complete the window host handshake: $stagedExePath"
        }
        $ready = $readyLine.Result | ConvertFrom-Json
        if ($ready.type -ne "ready" -or $ready.scale -le 0 -or $ready.width -le 0 -or $ready.height -le 0 -or $hostProcess.HasExited) {
            throw "FocusStage.exe returned an invalid window host handshake: $stagedExePath"
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

    $deployStage = Join-Path ([IO.Path]::GetTempPath()) ("FocusStage-deploy-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $deployStage | Out-Null
    try {
        Copy-Item -Path (Join-Path $WinPayloadDir "*") -Destination $deployStage -Recurse -Force
        Copy-Item -LiteralPath (Join-Path $ScriptsPayloadDir "uninstall.ps1") -Destination (Join-Path $deployStage "uninstall.ps1") -Force
        Copy-Item -LiteralPath (Join-Path $ScriptsPayloadDir "mcp-config.py") -Destination (Join-Path $deployStage "mcp-config.py") -Force
        [IO.File]::WriteAllText((Join-Path $deployStage "distro.txt"), $Distro + "`n")

        if ($isVirtualized) {
            Copy-DirectoryViaWsl -DistroName $Distro -SourceDir $deployStage -DestinationDir $InstallDir
        } else {
            New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
            Copy-Item -Path (Join-Path $deployStage "*") -Destination $InstallDir -Recurse -Force
        }
    } finally {
        Remove-Item -Recurse -Force -LiteralPath $deployStage -ErrorAction SilentlyContinue
    }

    $exePath = Join-Path $InstallDir "FocusStage.exe"
    $wslHostPath = Convert-ToWslPath -DistroName $Distro -WinPath $exePath
    wsl.exe -d $Distro -e test -f $wslHostPath
    if ($LASTEXITCODE -ne 0) {
        throw "Deployed FocusStage.exe was not found on the real filesystem at '$wslHostPath'."
    }
    $hostPathBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($wslHostPath))
    $registerHost = 'dir="${XDG_CONFIG_HOME:-$HOME/.config}/focusstage"; umask 077; mkdir -p "$dir" && printf %s ' + $hostPathBase64 + ' | base64 -d > "$dir/native-host-path"'
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
    & $stagedExePath --sync-shortcuts --distro $Distro
    if ($LASTEXITCODE -ne 0) {
        throw "FocusStage --sync-shortcuts failed."
    }
    if ($isVirtualized) {
        $virtualIconsDir = Join-Path $InstallDir "icons"
        if (Test-Path $virtualIconsDir) {
            $stagedIconsDir = Join-Path ([IO.Path]::GetTempPath()) ("FocusStage-icons-" + [guid]::NewGuid().ToString("N"))
            New-Item -ItemType Directory -Force -Path $stagedIconsDir | Out-Null
            try {
                Copy-Item -Path (Join-Path $virtualIconsDir "*") -Destination $stagedIconsDir -Recurse -Force
                Copy-DirectoryViaWsl -DistroName $Distro -SourceDir $stagedIconsDir -DestinationDir $virtualIconsDir
            } finally {
                Remove-Item -Recurse -Force -LiteralPath $stagedIconsDir -ErrorAction SilentlyContinue
            }
        }
    }

    if (-not $SkipMcpConfig) {
        Write-Host "Registering FocusStage MCP for Claude Code and Codex (Windows & WSL)..." -ForegroundColor Cyan
        $wslWinHome = Convert-ToWslPath -DistroName $Distro -WinPath $HOME
        $wslMcpHelper = Convert-ToWslPath -DistroName $Distro -WinPath (Join-Path $ScriptsPayloadDir "mcp-config.py")
        wsl.exe -d $Distro -e python3 -I $wslMcpHelper --windows-home $wslWinHome --distro $Distro
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to register the FocusStage MCP server."
        }
    }

    Write-Host ""
    Write-Host "FocusStage installed successfully!" -ForegroundColor Green
    Write-Host "  - WSL Distro       : $Distro"
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
} catch {
    Fail-Install $_.Exception.Message
}
