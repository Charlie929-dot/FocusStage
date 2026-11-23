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
    Clipboard · A11y(AT-SPI) · DisplaySink(NativeSink)
  focusstage mcp (Rust)
    默认返回原图，支持坐标操作；传入 som: true 可开启 SoM 控件打标
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
| **M4** | 原生 Win32 无标题栏宿主、显示后端单轨化、通用进程生命周期与内置 Stage 系统控制界面 | 进行中（原生宿主和显示后端收敛已完成） |
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
- 安装器会把 `focusstage` 注册到 Windows 用户的 Claude Code MCP 配置 `~/.claude.json`，使用 `-Distro` 指定的 WSL 发行版，默认是 Ubuntu。MCP 注册需要该发行版中的 `python3`。加 `-SkipMcpConfig` 可跳过注册。
- 运行 `powershell -ExecutionPolicy Bypass -File scripts/uninstall.ps1` 会卸载 FocusStage 并移除其用户级 MCP 条目。

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

# MCP 客户端入口（stdio）
wsl.exe -d Ubuntu -e /usr/local/bin/focusstage mcp
```

手动安装时，可以把以下配置保存为仓库根目录的 `.mcp.json`。使用其他 WSL 发行版时，把 `Ubuntu` 替换为对应名称。

```json
{
  "mcpServers": {
    "focusstage": {
      "type": "stdio",
      "command": "wsl.exe",
      "args": ["-d", "Ubuntu", "-e", "/usr/local/bin/focusstage", "mcp"]
    }
  }
}
```

打开已安装的 GUI 应用时，先用 `stage_list_apps` 查目录（可传 `query`），再把返回的 `id` 作为 `stage_launch` 的 `app_id`。也可以传唯一的 `app_name`。目录启动复用启动器登记的命令和参数；`app` 仍可用于显式运行命令。

- `stage_launch` 默认等待窗口出图，最多 8 秒（可用 `wait_ready_ms` 指定 1–20000 毫秒），就绪时附上同一窗口帧的原图。`observe: false` 只跳过截图。向已有 Stage 发送打开参数时，等待新窗口或活动窗口的画面更新。显式要求新窗口时，只接受新映射的窗口。
- `ready` 表示窗口已有可绘制的帧，不代表应用内容已加载完成。如果首张截图仍是空白内容区，可以用 `stage_observe` 获取新一帧。
- 等待超时返回 `timeout` 并保留 Stage，已知的启动进程退出状态会放在 `launcher_exit`。确认启动失败时返回 `launch_failed`。如果窗口已就绪但截图发生渲染错误，结果保留 `ready` 和 Stage ID，并在 `observe_message` 中说明错误。
- `stage_observe` 可只传 `stage_id`，不会获取租约或更改窗口显隐。`stage_act` 仍需租约；只想发送输入时可用 `observe: false` 跳过截图和默认等待。需要等待时可显式传 `settle: true`，并用 `settle_ms` 覆盖默认的 600 ms 上限。
- `stage_drop_files` 需要有效租约和 `paths`（Windows 盘符路径或 WSL 绝对路径）。可同时传 `x`、`y` 指定窗口内逻辑坐标，否则使用当前 Stage 指针位置。成功表示已发送拖放，不保证应用接受文件。

### 3. 运行 Windows 11 启动器与托盘（`FocusStage.exe`）

```powershell
dotnet run --project tray/FocusStageTray.csproj -c Release
```

默认全局接管热键为 `Ctrl+Alt+Space`，可立即作废所有 AI 租约并将后台 Stage 恢复至桌面。

安装器会在所选 WSL 发行版中登记 Windows 宿主。`focusstaged` 启动时连接原生 Win32 宿主，宿主不可用或显式传入 `--headless` 时进入离屏模式。可用 `focusstaged --native-host <WSL 中的 FocusStage.exe 路径>` 测试开发版本。`focusstage --json ping` 的 `desktop_active` 表示原生宿主是否已连接。

原生窗口保留 Linux 应用自己的标题控件，不叠加 Windows 标题栏。每个 Stage 可以有多个独立窗口，任务栏按 Stage 分组；关闭宿主窗口只向对应的 Linux 窗口发关闭请求，在内置系统控制面板（或托盘）中「关闭 Stage」仍是结束整个 Stage。跨显示器移动时，支持 `wp_fractional_scale_v1` 的 Wayland 客户端会收到该窗口的 DPI 更新；X11 客户端仍由 XWayland 路径缩放。

启动器的 **设置 → Linux 系统设置** 可选择默认浏览器和终端、同步用户级协议关联。Linux 应用通过 `xdg-open`、`gio open` 或 `focusstage open --ensure-daemon <URI>` 打开链接时，会复用已有的 Human Stage，或者启动目标应用。目标 Stage 被 Agent 租用、冻结或无法唯一确定时会返回错误。Windows 默认浏览器选项只接收 HTTP(S)。需要处理 `vscode://` 等 Linux 应用回调时，请使用 Linux 浏览器。

Chromium、Electron 和 Gecko 使用各自的 WSL 用户配置目录，关闭并重开后可以从原目录读取设置和 Cookie。再次启动已经由 FocusStage 托管的同款单实例应用时，会复用已有的 Human Stage。显式传入的 profile 参数会保留。

安装器提供 GNOME Keyring。VS Code 使用 Linux Secret Service 保存加密凭据时，首次使用可能提示创建带密码的密钥环，WSL 重启后可能需要重新解锁。若 VS Code 的 `argv.json` 显式设置了 `"password-store": "basic"`，VS Code 会按该设置选择存储后端。

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
