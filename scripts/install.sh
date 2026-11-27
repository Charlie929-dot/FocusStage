#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
DISTRO=""
ORIGINAL_ARGS=("$@")

KEEP_WINDOW_HOST=0
SKIP_RUST_BUILD=0
SKIP_TRAY_BUILD=0
NO_LAUNCH=0
SKIP_MCP_CONFIG=0

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [options]

Build and deploy FocusStage from WSL to both WSL (/usr/local) and Windows (%LOCALAPPDATA%\FocusStage).
Uses WSL interoperability and the Windows .NET SDK, but not PowerShell.

Options:
  --distro <name>       Target WSL distro (default: current distro)
  --keep-window-host    Leave running FocusStage processes alone (automatic in a Stage)
  --skip-rust-build     Skip cargo build --workspace --release
  --skip-tray-build     Skip dotnet publish of Windows tray/launcher
  --no-launch           Do not launch FocusStage.exe after deploying
  --skip-mcp-config     Do not update Claude Code / Codex MCP configurations
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

fail() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

windows_exe() {
  local candidate
  candidate=$(type -P "$1") || candidate="${WINDOWS_SYSTEM_DIR}/$1"
  [[ -f "$candidate" && -x "$candidate" ]] || fail "Cannot find $1. Enable WSL interoperability and make the Windows drive accessible."
  realpath -- "$candidate"
}

windows_env() {
  local reply="" key value
  reply=$(cd -- "$(dirname -- "$CMD_EXE")" && "$CMD_EXE" /d /u /c set "$1" </dev/null | iconv -f UTF-16LE -t UTF-8) || return 1
  while IFS='=' read -r key value; do
    if [[ "${key,,}" == "${1,,}" && -n "${value%$'\r'}" ]]; then
      printf '%s\n' "${value%$'\r'}"
      return
    fi
  done <<< "$reply"
  return 1
}

install_windows_files() (
  shopt -s globstar nullglob dotglob
  for source in "$BUILD_DIR/publish"/**/*; do
    [[ -f "$source" ]] || continue
    destination="$INSTALL_DIR/${source#"$BUILD_DIR/publish/"}"
    mkdir -p -- "$(dirname -- "$destination")"
    for old in "$destination".old.*; do
      rm -f -- "$old" 2>/dev/null || true
    done
    if ! mv -fT -- "$source" "$destination" 2>/dev/null; then
      backup="$destination.old.${BUILD_DIR##*.}"
      mv -T -- "$destination" "$backup" || fail "Close FocusStage before replacing $destination."
      if ! mv -T -- "$source" "$destination"; then
        mv -T -- "$backup" "$destination"
        return 1
      fi
    fi
  done
)

command -v wslpath >/dev/null || fail 'wslpath is required. Run this script inside WSL.'
WINDOWS_SYSTEM_DIR=$(wslpath -u 'C:\Windows\System32')
WSL_EXE=$(windows_exe wsl.exe)
CURRENT_DISTRO=${WSL_DISTRO_NAME:-}
if [[ -z "$CURRENT_DISTRO" ]]; then
  wsl_root=$(wslpath -w /)
  if [[ "$wsl_root" =~ ^\\\\(wsl\.localhost|wsl\$)\\([^\\]+)\\?$ ]]; then
    CURRENT_DISTRO=${BASH_REMATCH[2]}
  else
    fail 'Cannot identify the current WSL distro.'
  fi
fi
DISTRO=${DISTRO:-$CURRENT_DISTRO}
if [[ "${DISTRO,,}" != "${CURRENT_DISTRO,,}" ]]; then
  "$WSL_EXE" -d "$DISTRO" -e test -f "$SCRIPT_DIR/install.sh" </dev/null ||
    fail "Cannot find $SCRIPT_DIR/install.sh in '$DISTRO'. Run install.sh from that distro's checkout."
  exec "$WSL_EXE" -d "$DISTRO" -e bash -lc 'exec bash "$@"' bash "$SCRIPT_DIR/install.sh" "${ORIGINAL_ARGS[@]}"
fi

CMD_EXE=$(windows_exe cmd.exe)
local_app_data=$(windows_env LOCALAPPDATA) || fail 'Cannot read the Windows LOCALAPPDATA directory.'
local_app_data=$(wslpath -u "$local_app_data")
[[ -d "$local_app_data" ]] || fail "Windows LOCALAPPDATA is not accessible at $local_app_data."
INSTALL_DIR="$local_app_data/FocusStage"
HOST_EXE="$INSTALL_DIR/FocusStage.exe"
WSL_UID=$("$WSL_EXE" -d "$DISTRO" -e /usr/bin/id -u </dev/null) || fail "Cannot run commands in '$DISTRO' through WSL interop."
WSL_UID=${WSL_UID%$'\r'}
[[ "$WSL_UID" =~ ^[0-9]+$ ]] || fail "Cannot identify the default user in '$DISTRO'."

if ((! SKIP_TRAY_BUILD || ! SKIP_MCP_CONFIG)); then
  command -v python3 >/dev/null || fail 'python3 is required to verify the Windows build or update MCP settings.'
fi
if ((! SKIP_MCP_CONFIG)); then
  windows_home=$(windows_env USERPROFILE) || fail 'Cannot read the Windows USERPROFILE directory.'
  windows_home_wsl=$(wslpath -u "$windows_home")
fi
if ((SKIP_TRAY_BUILD)); then
  [[ -f "$HOST_EXE" ]] || fail "No installed Windows host at $HOST_EXE. Run without --skip-tray-build first."
else
  if ! dotnet_exe=$(type -P dotnet.exe); then
    program_files=$(windows_env ProgramFiles) || fail 'Cannot read the Windows Program Files directory.'
    dotnet_exe="$(wslpath -u "$program_files")/dotnet/dotnet.exe"
  fi
  [[ -x "$dotnet_exe" ]] || fail 'Install the Windows .NET 8 SDK before building the tray.'
  dotnet_exe=$(realpath -- "$dotnet_exe")
fi

if [[ -n "${FOCUSSTAGE_STAGE_ID:-}" ]] && ((! KEEP_WINDOW_HOST)); then
  echo 'Running inside a Stage; keeping running FocusStage processes alive.'
  KEEP_WINDOW_HOST=1
fi
if ((! KEEP_WINDOW_HOST)); then
  taskkill_exe=$(windows_exe taskkill.exe)
fi

echo '[1/5] Preparing Linux binaries...'
if ((! SKIP_RUST_BUILD)); then
  (cd -- "$REPO_ROOT" && cargo build --workspace --release)
fi
for binary in focusstage focusstaged focusstage-tun; do
  [[ -x "$REPO_ROOT/target/release/$binary" ]] || fail "Missing target/release/$binary. Build without --skip-rust-build first."
done

echo '[2/5] Preparing Windows binaries...'
if ((! SKIP_TRAY_BUILD)); then
  BUILD_DIR=$(mktemp -d "$local_app_data/FocusStage-build.XXXXXXXX")
  trap 'rm -rf -- "$BUILD_DIR"' EXIT
  tar -C "$REPO_ROOT" --exclude=tray/bin --exclude=tray/obj -cf - tray | tar -C "$BUILD_DIR" -xf -
  (cd -- "$BUILD_DIR/tray" && "$dotnet_exe" publish FocusStageTray.csproj -c Release -o ../publish --nologo </dev/null)
  python3 -I "$SCRIPT_DIR/check-window-host.py" "$BUILD_DIR/publish/FocusStage.exe"
fi

if ((! KEEP_WINDOW_HOST)); then
  "$WSL_EXE" -d "$DISTRO" -e bash -c 'focusstage shutdown || pkill -u "$(id -u)" -x focusstaged' </dev/null >/dev/null 2>&1 || true
  "$taskkill_exe" /f /im FocusStage.exe /im FocusStageTray.exe </dev/null >/dev/null 2>&1 || true
fi

echo '[3/5] Installing Linux binaries and desktop components...'
"$WSL_EXE" -d "$DISTRO" -u root -e bash -se -- "$REPO_ROOT" "$WSL_UID" <<'WSL_ROOT'
repo=$1
install -d -m 755 /usr/local/libexec
install -m 755 "$repo/target/release/focusstage" "$repo/target/release/focusstaged" /usr/local/bin/
install -m 755 "$repo/target/release/focusstage-tun" /usr/local/libexec/
for helper in focusstage-launch focusstage-open; do
  sed 's/\r$//' "$repo/scripts/$helper" > "/usr/local/bin/$helper"
  chmod 755 "/usr/local/bin/$helper"
done
install -d -m 755 /etc/polkit-1/rules.d
sed 's/\r$//' "$repo/scripts/49-focusstage-gvfs-admin.rules" > /etc/polkit-1/rules.d/49-focusstage-gvfs-admin.rules
chmod 644 /etc/polkit-1/rules.d/49-focusstage-gvfs-admin.rules
packages=(python3 pkexec gnome-keyring xdg-desktop-portal xdg-desktop-portal-gtk xdg-utils desktop-file-utils xwayland fonts-noto-cjk iproute2 libcap2-bin)
if command -v apt-get >/dev/null && ! dpkg -s "${packages[@]}" >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-upgrade "${packages[@]}"
  fc-cache -f >/dev/null
fi
setcap cap_net_admin,cap_dac_override=ep /usr/local/libexec/focusstage-tun
if [[ -f /usr/local/libexec/focusstage-auth ]]; then
  /usr/bin/python3 -I /usr/local/libexec/focusstage-auth remove
fi
/usr/local/bin/focusstage --json sync-apps --uid "$2" >/dev/null
WSL_ROOT

echo '[4/5] Installing Windows binaries...'
if ((! SKIP_TRAY_BUILD)); then
  install_windows_files
fi
printf '%s\n' "$DISTRO" > "$INSTALL_DIR/distro.txt"

echo '[5/5] Registering the Windows host and syncing shortcuts...'
"$WSL_EXE" -d "$DISTRO" -e bash -se -- "$HOST_EXE" <<'WSL_USER'
dir="${XDG_CONFIG_HOME:-$HOME/.config}/focusstage"
umask 077
mkdir -p -- "$dir"
printf '%s' "$1" > "$dir/native-host-path"
if command -v gsettings >/dev/null; then
  if gsettings writable org.gnome.desktop.wm.preferences button-layout 2>/dev/null | grep -qx true; then
    gsettings set org.gnome.desktop.wm.preferences button-layout ':minimize,maximize,close' || true
  fi
  if gsettings writable org.gnome.desktop.interface toolkit-accessibility 2>/dev/null | grep -qx true; then
    gsettings set org.gnome.desktop.interface toolkit-accessibility true || true
  fi
fi
FOCUSSTAGE_SOCKET=/dev/null /usr/local/bin/focusstage --json settings init-defaults >/dev/null
WSL_USER
(
  cd -- "$INSTALL_DIR"
  FOCUSSTAGE_SOCKET=/dev/null WSLENV="FOCUSSTAGE_SOCKET${WSLENV:+:$WSLENV}" \
    "$HOST_EXE" --sync-shortcuts --distro "$DISTRO" </dev/null
)
if ((! SKIP_MCP_CONFIG)); then
  python3 -I "$SCRIPT_DIR/mcp-config.py" --windows-home "$windows_home_wsl" --distro "$DISTRO"
fi

printf '\nFocusStage installed: %s\n' "$HOST_EXE"
if ((KEEP_WINDOW_HOST)); then
  echo 'Restart running FocusStage processes when ready to load the update.'
fi
if ((! NO_LAUNCH)); then
  (cd -- "$INSTALL_DIR" && exec "$HOST_EXE" --distro "$DISTRO" </dev/null >/dev/null 2>&1) &
fi
