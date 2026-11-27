# Changelog

All notable changes to FocusStage are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-27

First public release of **FocusStage**.

### Added

- **WSL2 Wayland Compositor (`focusstaged`)**:
  - Per-Stage virtual focus isolation and lease-based input router (`Human` / `Agent` / `Frozen`) so humans and AI agents can operate separate windows concurrently without stealing focus.
  - Built-in rootless `Xwayland` per Stage for X11, Chromium, and Electron apps, with WSL2 Mesa D3D12 hardware acceleration support and GTK4 Cairo fallback on WSL2.
  - Offscreen rendering core supporting seamless transitions between Desktop and Hidden states while keeping app state and login sessions intact.
  - Native clipboard bridge, Fcitx5 / `text-input-v3` IME support, native cursors, fractional DPI scaling, and bidirectional Windows ↔ Wayland drag-and-drop.
  - Unified Ubuntu user session environment (`SessionEnv`) with D-Bus / `systemd --user` activation, GNOME Keyring integration, and XDG URI/protocol routing (`xdg-open`, `focusstage open`).
  - Transparent Windows proxy bridge (`focusstage-tun` powered by `smoltcp`).
- **Windows 11 Launcher, Tray & Native Window Host (`FocusStage.exe`)**:
  - Frameless Direct2D Win32 window host (`--window-host`) with optional LZ4 frame compression, per-Stage taskbar grouping, and live AI takeover overlay (glow border, virtual pointer, takeover pill).
  - WebView2 desktop launcher (`应用桌面`) with app discovery, folders, dock, shortcut sync to Start Menu/Desktop, and app uninstallation.
  - Stage Center (`舞台中心`) for live stage visibility/takeover management, daemon & global hotkey control (`Ctrl+Alt+Space`), Linux theme/font/Han-fallback settings, default apps & protocol health, WSL masking toggle, and WSL2 graphics driver / D3D12 GPU adapter selection (with DXGI hardware GPU enumeration).
  - Windows Hello + DPAPI credential vault and native Win32 Polkit elevation dialog for Linux administrative prompts and GNOME Keyring unlock.
- **CLI & MCP Server (`focusstage`)**:
  - Full CLI (`launch`, `list-apps`, `open`, `settings`, `sync-apps`, `uninstall-app`, `ping`, `shutdown`) and MCP stdio server (`focusstage mcp`) with optional Set-of-Mark (`som: true`) annotations.
  - Ready-to-use Linux project configs for Claude Code (`.mcp.json`) and Codex (`.codex/config.toml`), plus automatic Windows Claude Code MCP registration during install.
- **Packaging & Release Automation**:
  - Prebuilt Windows installer (`FocusStage-Setup-v0.1.0.exe` via Inno Setup) and portable ZIP (`FocusStage-v0.1.0-win-x64.zip`) with automatic WSL2/Ubuntu detection, missing runtime package installation, and clean Windows uninstall support.
