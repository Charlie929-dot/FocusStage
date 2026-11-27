# Security Policy

## Scope

FocusStage bridges Windows 11 and WSL2 Linux desktop sessions and interacts with several security-sensitive components:

- Windows Hello / DPAPI credential vault (`tray/CredentialVault.cs`)
- Native Win32 Polkit elevation dialog (`tray/NativeElevationDialog.cs`, `compositor/src/polkit.rs`)
- Privileged network bridge binary (`/usr/local/libexec/focusstage-tun` with `cap_net_admin,cap_dac_override`)
- MCP stdio server (`focusstage mcp`) and Wayland compositor IPC (`focusstaged`)

## Supported Versions

| Version | Supported |
| ------- | --------- |
| `0.1.x` | Yes       |

## Reporting a Vulnerability

If you discover a security vulnerability, **please do not open a public issue with exploit details**.

Instead, please report it privately via [GitHub Security Advisories](https://github.com/Charlie929-dot/FocusStage/security/advisories/new) on this repository. Include:

1. A clear description of the vulnerability and its potential impact.
2. Minimal steps to reproduce or a proof of concept.
3. Affected component(s) and environment details (Windows version, WSL distro, FocusStage version).

We will acknowledge receipt and work with you on a fix and coordinated disclosure.
