# FocusStage

**FocusStage** 让人和 AI 同时使用同一台电脑、互不抢焦点。

它是一个跑在 WSL2 里的 **Wayland 合成器**：被它托管的应用平时像普通窗口一样出现在 Win11 桌面上；需要时一键「收入后台」交给 AI，AI 独立操作它，而你的鼠标、键盘、正在打字的窗口和剪贴板**完全不受影响**。

> **正常使用（桌面窗口） → 一键收入后台 → AI 独立操作（可多窗口并发） → 按热键接管，窗口原位恢复**

---

## 它是怎么做到的

传统桌面只有一个前台焦点，所以 AI 操作窗口必须抢走你的鼠标。FocusStage 换了一个思路：

- **存在方式像原生安装软件**：Linux 应用通过图形化启动器、Windows 开始菜单和任务栏快捷方式统一启动与管理，并以无冗余标题栏、原生任务栏分组的 Win32 窗口（`NativeHost`）呈现；软件本身保持 Linux 原貌，不强行改造内部交互。
- **每个托管应用是一个 Stage（舞台）**，对应一个独立隔离的应用实例。
- **所有 Stage 永远处于"前台"**：合成器让每个应用都以为自己拥有焦点，再由合成器决定每个按键、每次点击实际送给谁。
- **状态切换对应用透明**：收入后台、交给 AI、收回桌面，都只是合成器内部改了路由表和显示开关，应用始终在运行、始终在渲染，画面与登录态完全连续。
- **内置系统控制中心 + 人永远优先**：FocusStage 的核心能力（查看运行中 Stage 状态、前后台与 AI 租约监控、一键隐藏/恢复/接管与守护进程管理）作为启动器内的一个内置系统软件呈现（由当前「设置」界面演进）；任何时候按下接管热键，AI 的操作权立即作废。

详细设计见 [DESIGN.md](DESIGN.md)。

---

## 架构一览

```text
Windows 11
  AI Agent ──MCP(stdio)──► wsl.exe -e focusstage mcp
  FocusStage.exe（UI 壳 + 原生宿主）
   · 图形化启动器：应用网格 / 文件夹 / 快捷方式同步 / 系统应用中心入口
   · 内置系统软件（设置与舞台监视）：Stage 状态 / AI 租约 / 一键接管与显隐
   · 托盘后台常驻：全局接管热键
   · --window-host：每个 Linux 顶层窗口对应一个无标题栏 Win32 HWND
             ▲ BGRA 帧（WSL 互操作 stdio）▼ 键鼠、窗口状态和 DPI
WSL2
  focusstaged (Rust + Smithay)
    Stage Manager · Router(虚拟焦点+租约) · Offscreen Core
    Clipboard · A11y(AT-SPI) · DisplaySink(主线 NativeSink；WSLg 过渡待清理)
  focusstage mcp (Rust)
    纯视觉 Set-of-Mark (SoM) 控件打标 + 双字母短码/坐标操作 (MarkScreen 方案)
        │
        ├─ Chromium / Electron 应用 (Wayland)
        └─ X11 应用 (每个 Stage 一个 rootless Xwayland)
```

---

## 适用范围与边界

- **支持**：浏览器与 Web 应用、开发工具，以及有 Linux 版本的桌面客户端（微信、QQ、飞书、钉钉、WPS、VS Code 等）。
- **前提**：应用必须通过 FocusStage 启动，不能中途接管已经在 Win11 上运行的 `.exe`。
- **分工与边界**：
  - 只有 Windows 版本的原生软件交由 MarkScreen 等外层方案处理；纯网页后台自动化可与 Playwright / Chrome DevTools MCP 互补；
  - FocusStage 不自研 Linux 应用商店或包格式安装器：应用安装交给 Ubuntu 应用中心 / `apt` 等标准工具，启动器基于标准 `.desktop` 自动发现并同步到 Windows。

---

## 路线

| 里程碑 | 目标 | 状态 |
| :--- | :--- | :--- |
| **M0** | 无头合成器验证多焦点隔离：Chromium + 终端 + X11 应用同时输入互不干扰 | 已实现并验证 |
| **M1** | MCP 接入，AI 在后台并发操作多个 Stage | 已实现并验证 |
| **M2** | 嵌套 WSLg 显示到桌面，打通「桌面 ↔ 后台 ↔ 原位恢复」 | 已实现（已转向原生宿主） |
| **M3** | 剪贴板互通、中文输入（`text-input-v3` / Fcitx5）、高 DPI、文件拖放与启动器 | 已实现并验证 |
| **M4** | 原生 Win32 无标题栏宿主、单轨收敛（淘汰 WSLg 后端）、通用进程生命周期与内置 Stage 系统控制界面 | 进行中（原生宿主已落地，收敛加固中） |
| M5 | 脱离 Windows 独立开机 | 远期 |

---

## 快速开始

### 1. 一键安装（推荐）

在 Windows PowerShell 中运行安装器，会自动构建并安装 WSL2 端（`focusstaged` / `focusstage` / `focusstage-launch`）、发布 Windows 启动器与托盘程序 `FocusStage.exe` 到 `%LOCALAPPDATA%\FocusStage`，并同步所有 Linux 应用到 Windows 开始菜单及桌面入口：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/install.ps1
```

安装完成后：
- 双击桌面或开始菜单中的 **FocusStage** 打开统一应用启动器（关闭窗口会自动缩回右下角托盘）。
- 开始菜单 `Programs\FocusStage\` 下会自动生成全部 Linux 应用的独立快捷方式（带图标），可右键“固定到开始屏幕”或“固定到任务栏”。
- 如需卸载可运行 `powershell -ExecutionPolicy Bypass -File scripts/uninstall.ps1`。

### 2. 手动构建与 CLI / MCP 用法

```bash
cargo build --workspace --release
sudo install -m 755 target/release/focusstaged /usr/local/bin/focusstaged
sudo install -m 755 target/release/focusstage /usr/local/bin/focusstage
sudo install -m 755 scripts/focusstage-launch /usr/local/bin/focusstage-launch

# 为每个 Linux 应用生成 FocusStage 桌面入口，使 Win11 开始菜单 / 任务栏固定直接走 FocusStage
sudo focusstage sync-apps

# 列出已发现的 Linux GUI 应用（含图标路径）
focusstage --json list-apps

# 启动不同类型 Stage（加 --ensure-daemon 自动拉起守护进程）
focusstage launch --ensure-daemon foot
focusstage launch --ensure-daemon --kind x11 xterm
focusstage launch --ensure-daemon --kind chromium chromium

# MCP 客户端配置（stdio）
wsl.exe -d Ubuntu -e focusstage mcp
```

### 3. 运行 Windows 11 启动器与托盘（`FocusStage.exe`）

```powershell
dotnet run --project tray/FocusStageTray.csproj -c Release
```

默认全局接管热键为 `Ctrl+Alt+Space`，可立即作废所有 AI 租约并将后台 Stage 恢复至桌面。

安装器会在所选 WSL 发行版中登记 Windows 宿主。`focusstaged` 默认使用原生宿主，现阶段暂保留 `focusstaged --wslg` 作为应急回退（计划在原生宿主稳定后移除）；也可用 `focusstaged --native-host <WSL 中的 FocusStage.exe 路径>` 测试开发版本。

原生窗口保留 Linux 应用自己的标题控件，不叠加 Windows 标题栏。每个 Stage 可以有多个独立窗口，任务栏按 Stage 分组；关闭宿主窗口只向对应的 Linux 窗口发关闭请求，在内置系统控制面板（或托盘）中「关闭 Stage」仍是结束整个 Stage。跨显示器移动时，支持 `wp_fractional_scale_v1` 的 Wayland 客户端会收到该窗口的 DPI 更新；X11 客户端仍由 XWayland 路径缩放。

---

## 目录结构

```text
FocusStage/
├── README.md        # 项目概述与快速开始
├── DESIGN.md        # 架构设计与收敛路线
├── compositor/      # [WSL2] focusstaged：Rust + Smithay 合成器、Router、Stage Manager、DisplaySink
├── mcp/             # [WSL2] focusstage：CLI 与 MCP 服务（通过 wsl.exe stdio 对外）
├── tray/            # [Win11] .NET 8 交互壳：图形化启动器、内置系统信息/控制界面、托盘热键与原生 Win32 宿主
└── scripts/         # 安装 / 卸载与启动辅助脚本
```

---

## 运行环境

- **宿主机**：Windows 11，已具备 `.NET 8`、`Git`
- **子系统**：WSL2 + `Ubuntu 26.04.1 LTS`
- **架构与收敛细节**：见 [DESIGN.md](DESIGN.md)
