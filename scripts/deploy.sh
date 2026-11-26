#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
DISTRO="${WSL_DISTRO_NAME:-Ubuntu}"

KEEP_WINDOW_HOST=0
SKIP_RUST_BUILD=0
SKIP_TRAY_BUILD=0
NO_LAUNCH=0
SKIP_MCP_CONFIG=0

usage() {
  cat <<'EOF'
Usage: scripts/deploy.sh [options]

Build and deploy FocusStage from WSL to both WSL (/usr/local) and Windows (%LOCALAPPDATA%\FocusStage).

Options:
  --distro <name>       Target WSL distro (default: $WSL_DISTRO_NAME or Ubuntu)
  --keep-window-host    Keep running focusstaged and FocusStage.exe --window-host alive
                        (safe to run from an app hosted inside FocusStage)
  --skip-rust-build     Skip cargo build --workspace --release
  --skip-tray-build     Skip dotnet publish of Windows tray/launcher
  --no-launch           Do not launch FocusStage.exe after deploying
  --skip-mcp-config     Do not update ~/.claude.json MCP configuration
  -h, --help            Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --distro)
      DISTRO="${2:?--distro requires a value}"
      shift 2
      ;;
    --keep-window-host)
      KEEP_WINDOW_HOST=1
      shift
      ;;
    --skip-rust-build)
      SKIP_RUST_BUILD=1
      shift
      ;;
    --skip-tray-build)
      SKIP_TRAY_BUILD=1
      shift
      ;;
    --no-launch)
      NO_LAUNCH=1
      shift
      ;;
    --skip-mcp-config)
      SKIP_MCP_CONFIG=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! command -v powershell.exe >/dev/null 2>&1 || ! command -v wslpath >/dev/null 2>&1; then
  echo "Error: powershell.exe / wslpath not available (WSL interop is required)." >&2
  exit 1
fi

INSTALL_PS1_WIN="$(wslpath -w "${REPO_ROOT}/scripts/install.ps1")"
PS_ARGS=(-NoProfile -ExecutionPolicy Bypass -File "$INSTALL_PS1_WIN" -Distro "$DISTRO" -WslRepo "$REPO_ROOT")

if [[ "$KEEP_WINDOW_HOST" -eq 1 ]]; then
  PS_ARGS+=(-KeepWindowHost)
fi
if [[ "$SKIP_RUST_BUILD" -eq 1 ]]; then
  PS_ARGS+=(-SkipRustBuild)
fi
if [[ "$SKIP_TRAY_BUILD" -eq 1 ]]; then
  PS_ARGS+=(-SkipTrayBuild)
fi
if [[ "$NO_LAUNCH" -eq 1 ]]; then
  PS_ARGS+=(-NoLaunch)
fi
if [[ "$SKIP_MCP_CONFIG" -eq 1 ]]; then
  PS_ARGS+=(-SkipMcpConfig)
fi

exec powershell.exe "${PS_ARGS[@]}" </dev/null
