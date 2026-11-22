# FocusStage

**FocusStage** 让人和 AI 同时使用同一台电脑、互不抢焦点。

它是一个跑在 WSL2 里的 **Wayland 合成器**：被它托管的应用平时像普通窗口一样出现在 Win11 桌面上；需要时一键「收入后台」交给 AI，AI 独立操作它，而你的鼠标、键盘、正在打字的窗口和剪贴板**完全不受影响**。

> **正常使用（桌面窗口） → 一键收入后台 → AI 独立操作（可多窗口并发） → 按热键接管，窗口原位恢复**

---

## 它是怎么做到的

传统桌面只有一个前台焦点，所以 AI 操作窗口必须抢走你的鼠标。FocusStage 换了一个思路：

- **每个托管应用是一个 Stage（舞台）**，对应一个独立进程。
- **所有 Stage 永远处于"前台"**：合成器让每个应用都以为自己拥有焦点，再由合成器决定每个按键、每次点击实际送给谁。
- **状态切换对应用透明**：收入后台、交给 AI、收回桌面，都只是合成器内部改了路由表和显示开关，应用始终在运行、始终在渲染，画面与登录态完全连续。
- **人永远优先**：任何时候按下接管热键，AI 的操作权立即作废。

详细设计见 [DESIGN.md](DESIGN.md)。

---

## 架构一览

```text
Windows 11
  AI Agent ──MCP(stdio)──► wsl.exe -e focusstage mcp
  FocusStage Tray：全局接管热键 / Stage 列表

WSL2
  focusstaged (Rust + Smithay)
    Stage Manager · Router(虚拟焦点+租约) · Offscreen Core
    Clipboard · A11y(AT-SPI) · DisplaySink(第一版嵌套进 WSLg)
        │
        ├─ Chromium / Electron 应用 (Wayland)
        └─ X11 应用 (每个 Stage 一个 rootless Xwayland)
```

---

## 适用范围

- **支持**：浏览器与 Web 应用、开发工具，以及有 Linux 版本的桌面客户端（微信、QQ、飞书、钉钉、WPS、VS Code 等）。
- **前提**：应用必须通过 FocusStage 启动，不能中途接管已经在 Win11 上运行的 `.exe`。
- **不覆盖**：只有 Windows 版本的原生软件（交由 MarkScreen 等外层方案处理）。

---

## 路线

| 里程碑 | 目标 | 状态 |
| :--- | :--- | :--- |
| **M0** | 无头合成器验证多焦点隔离：Chromium + 终端 + X11 应用同时输入互不干扰 | 已实现并验证 |
| **M1** | MCP 接入，AI 在后台并发操作多个 Stage | 已实现并验证 |
| **M2** | 嵌套 WSLg 显示到桌面，打通「桌面 ↔ 后台 ↔ 原位恢复」 | 已实现并验证 |
| **M3** | 剪贴板互通、中文输入（`text-input-v3` / Fcitx5）、高 DPI、文件拖放 | 已实现并验证 |
| M4 / M5 | 按需：原生宿主壳 / 脱离 Windows 独立开机 | 远期 |

---

## 快速开始

### 1. 构建并安装 WSL2 端（`focusstaged` + `focusstage`）

```bash
CARGO_TARGET_DIR=$HOME/.cache/focusstage-target cargo build --workspace
sudo cp $HOME/.cache/focusstage-target/debug/focusstaged /usr/local/bin/focusstaged
sudo cp $HOME/.cache/focusstage-target/debug/focusstage /usr/local/bin/focusstage
```

### 2. 启动合成器与使用 CLI / MCP

```bash
# 启动带 WSLg 窗口呈现的合成器（无头调试可加 --headless）
focusstaged

# 启动不同类型 Stage
focusstage launch foot
focusstage launch --kind x11 xterm
focusstage launch --kind chromium chromium

# MCP 客户端配置（stdio）
wsl.exe -d Ubuntu -e focusstage mcp
```

### 3. 构建并运行 Windows 11 托盘（`FocusStageTray`）

```powershell
dotnet build tray/FocusStageTray.csproj -c Release
dotnet run --project tray/FocusStageTray.csproj -c Release
```

默认全局接管热键为 `Ctrl+Alt+Space`，可立即作废所有 AI 租约并将后台 Stage 恢复至桌面。

---

## 规划目录结构

```text
FocusStage/
├── README.md        # 项目概述
├── DESIGN.md        # 架构设计
├── compositor/      # [WSL2] focusstaged：Rust + Smithay 合成器、Router、Stage Manager
├── mcp/             # [WSL2] MCP 服务（通过 wsl.exe stdio 对外）
└── tray/            # [Win11] .NET 8 托盘：全局接管热键与 Stage 列表
```

---

## 运行环境

- **宿主机**：Windows 11，已具备 `.NET 8`、`Git`
- **子系统**：WSL2 + `Ubuntu 26.04.1 LTS`，已启用 `WSLg`
- **开发起步**：从 [DESIGN.md](DESIGN.md) 的 **M0** 开始
