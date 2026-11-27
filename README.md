# FocusStage

[![Release](https://img.shields.io/github/v/release/Charlie929-dot/FocusStage?style=flat-square)](https://github.com/Charlie929-dot/FocusStage/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/Charlie929-dot/FocusStage/ci.yml?branch=master&label=CI&style=flat-square)](https://github.com/Charlie929-dot/FocusStage/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%2011%20%2B%20WSL2-0078D4?style=flat-square)](#运行环境)

**FocusStage** 让人和 AI 同时使用同一台电脑、互不抢焦点。

> *Run Linux GUI apps natively on Windows 11 with per-app virtual focus — let humans and AI agents share the same desktop concurrently without stealing focus.*

它是一个跑在 WSL2 里的 **Wayland 合成器**：被它托管的应用平时像普通窗口一样出现在 Win11 桌面上；需要时一键「收入后台」交给 AI，AI 独立操作它，而你的鼠标、键盘、正在打字的窗口和剪贴板**完全不受影响**。

> **正常使用（桌面窗口） → 一键收入后台 → AI 独立操作（可多窗口并发） → 按热键接管，窗口原位恢复**

**普通用户直接上手**：前往 [GitHub Releases](https://github.com/Charlie929-dot/FocusStage/releases/latest) 下载 `FocusStage-Setup-v*.exe` 双击安装即可，无需配置 Rust 或 `.NET SDK`（详见下方 [快速开始](#快速开始)）。

---

## 它是怎么做到的

传统桌面只有一个前台焦点，所以 AI 操作窗口必须抢走你的鼠标。FocusStage 换了一个思路：

- **存在方式像原生安装软件**：Linux 应用通过图形化启动器、Windows 开始菜单和任务栏快捷方式统一启动与管理，并以无冗余标题栏、原生任务栏分组的 Win32 窗口（`NativeHost`）呈现；软件本身保持 Linux 原貌，不强行改造内部交互。
- **每个托管应用是一个 Stage（舞台）**，对应一个独立隔离的应用实例。
- **所有 Stage 永远处于"前台"**：合成器让每个应用都以为自己拥有焦点，再由合成器决定每个按键、每次点击实际送给谁。
- **状态切换对应用透明**：收入后台、交给 AI、收回桌面，都只是合成器内部改了路由表和显示开关，应用始终在运行、始终在渲染，画面与登录态完全连续。
- **内置「舞台中心」+ 人永远优先**：FocusStage 的核心控制能力（运行舞台状态观测、前后台与 AI 租约监控、一键隐藏/恢复/接管与关闭、守护进程管理与 Linux 系统设置）以启动器顶层的独立「舞台中心」视图呈现；任何时候按下接管热键，或直接点击桌面 AI 操控窗口顶部胶囊中的「接管」按钮，AI 的操作权立即作废。

详细设计见 [DESIGN.md](DESIGN.md)。

---

## 架构一览

```text
Windows 11
  AI Agent ──MCP(stdio)──► wsl.exe -e focusstage mcp
  FocusStage.exe（WebView2 启动器 + Direct2D 原生宿主）
   · 应用桌面：应用网格 / 文件夹 / 快捷栏 Dock / 主题外观与右键卸载 / 快捷方式同步
   · 舞台中心：运行舞台显隐与接管 / 守护进程与热键 / Linux 默认应用与协议关联 / 外观设置
   · 托盘后台常驻：全局接管热键 / 动态浏览器快速启动 / 剪贴板同步开关
   · --window-host：无标题栏 Direct2D Win32 HWND + AI 接管覆盖层（内发光边框 / 虚拟指针 / 接管胶囊）+ 原生 Polkit 提权弹窗
             ▲ BGRA / LZ4 帧（WSL 互操作 stdio）▼ 键鼠、窗口状态和 DPI
WSL2
  focusstaged (Rust + Smithay)
    Stage Manager · Router(虚拟焦点+租约) · Offscreen Core · SessionEnv · 原生 Polkit 代理
    Clipboard · A11y(AT-SPI) · XDG 协议路由 & 图标主题 · DisplaySink(NativeSink)
  focusstage mcp (Rust)
    默认返回原图，支持坐标操作；传入 som: true 可开启 SoM 控件打标
        │
        ├─ Wayland / Gecko 应用
        └─ Chromium / Electron (WSL2 默认内置 XWayland + D3D12) / X11 应用 (每 Stage 独立 Xwayland)
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
| **M4** | 原生 Win32 无标题栏宿主、Direct2D/LZ4 与 XWayland/D3D12、WebView2 启动器与「舞台中心」、AI 接管覆盖层、统一会话授权与 Release 发版流 | 核心闭环已实现（剩余增强项按需推进） |
| M5 | 脱离 Windows 独立开机 | 远期 |

---

## 快速开始

### 1. 安装与部署

#### 方式 A：直接安装预编译发行版（推荐给普通用户）

无需在本地安装 Rust 或 `.NET SDK`，前往 [GitHub Releases](https://github.com/Charlie929-dot/FocusStage/releases/latest) 下载 `FocusStage-Setup-<tag>.exe` 双击运行即可（也可解压绿色包 `FocusStage-<tag>-win-x64.zip` 后运行 `install-release.ps1`）：

- 自动检测本机 WSL2 与 Ubuntu 发行版（不写死发行版名称，缺失时可引导安装 `Ubuntu-24.04`，多发行版时支持选择）；
- 通过 `apt-get install --no-upgrade` 仅补齐缺失的桌面组件与运行库（`libpixman-1-0`、`libwayland-server0`、`libxkbcommon0`、`libffi8` 等）；
- 部署预编译 `focusstaged` / `focusstage` 与自包含 `FocusStage.exe` 到 `%LOCALAPPDATA%\FocusStage`（自动旁路 MSIX 文件系统虚拟化），登记宿主路径、同步快捷方式并自动为 Windows 与 WSL 双侧的 Claude Code / Codex 注册全局 MCP，支持在 Windows「应用和功能」中直接卸载。

#### 方式 B：从源码一键构建部署（开发者）

**从 WSL 安装（不使用 PowerShell）**：在 WSL 终端的仓库根目录运行以下命令，可以同时安装 Linux 端和 Windows 启动器。需要启用 WSL 互操作，准备好 Rust 工具链、WSL 中的 `python3` 和 Windows 上的 .NET 8 SDK：

```bash
./scripts/install.sh
# 保留正在运行的 FocusStage 会话（在托管窗口内运行时自动保留）：
./scripts/install.sh --keep-window-host
```

脚本会安装 Linux 端及所需桌面组件，并构建、验证和部署 `FocusStage.exe` 到 `%LOCALAPPDATA%\FocusStage`，初始化 Linux 协议关联与 Windows 快捷方式。保留会话时，需要之后重启 FocusStage 才能使用新版。可用 `./scripts/install.sh --help` 查看其他选项。

**从 Windows 安装**：在仓库根目录从 PowerShell 运行开发安装器，不需要批处理包装脚本：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install.ps1
```

Windows 安装器可用 `-Distro` 选择发行版，或用 `-WslRepo` 指定该发行版里的源码路径。

安装完成后：
- 双击桌面或开始菜单中的 **FocusStage** 打开统一应用启动器（包含「应用桌面」与「舞台中心」两个顶层视图）。
- 需要提权或解锁密钥环时，FocusStage 会弹出原生 Win32 Polkit / 解锁弹窗；你也可以在 **舞台中心 → 账号与凭据** 中管理 WSL Ubuntu 账号，并通过 **Windows Hello（DPAPI 加密托管）** 保护 Linux / GNOME Keyring 密码（不会明文写入 WSL），实现刷脸/指纹/PIN 一键授权与解锁。
- 开始菜单 `Programs\FocusStage\` 下会自动生成全部 Linux 应用的独立快捷方式（按 Linux 图标主题解析并带图标），可右键“固定到开始屏幕”或“固定到任务栏”。
- 安装器会自动将 `focusstage` 注册为用户级全局 MCP（无需仓库级 `.mcp.json` / `.codex/config.toml`，在任意工作目录均可使用）：
  - **Windows 侧**（`%USERPROFILE%\.claude.json`、`%USERPROFILE%\.codex\config.toml`）：通过 `wsl.exe -d <发行版> -e /usr/local/bin/focusstage mcp` 转接；
  - **WSL 侧**（`~/.claude.json`、`~/.codex/config.toml`）：直接调用原生 `/usr/local/bin/focusstage mcp`。
  `install.sh` 默认使用当前发行版，可用 `--distro` 指定；如不需要自动写入 MCP 配置，可用 `--skip-mcp-config`（Windows 安装器用 `-SkipMcpConfig`）。
- 运行 `powershell -ExecutionPolicy Bypass -File scripts/uninstall.ps1` 会自动读取部署时记录的 `distro.txt`（或使用 `-Distro`），同步清理 Windows 与 WSL 双侧 Claude Code / Codex 的 `focusstage` MCP 条目、移除协议关联、授权配置与快捷方式并删除安装目录（Inno Setup 卸载器传入 `-KeepInstallDir` 后由安装器自身清理目录）。

### 2. 手动构建与 CLI / MCP 用法

```bash
cargo build --workspace --release
sudo install -m 755 target/release/focusstaged /usr/local/bin/focusstaged
sudo install -m 755 target/release/focusstage /usr/local/bin/focusstage
sudo install -m 755 scripts/focusstage-launch /usr/local/bin/focusstage-launch
sudo install -m 755 scripts/focusstage-open /usr/local/bin/focusstage-open

# 为每个 Linux 应用生成 FocusStage 桌面入口，并初始化用户级默认应用与协议关联
sudo focusstage sync-apps
focusstage settings init-defaults

# 列出已发现的 Linux GUI 应用（含按 Linux 图标主题解析的图标路径）
focusstage --json list-apps

# 卸载已发现的 Linux 应用（apt/snap/flatpak），或删除下载目录里的 .deb/.AppImage 安装包
focusstage --json uninstall-app --app-id <focusstage-...>

# 启动不同类型 Stage（--kind 支持 wayland / chromium / electron / gecko / x11；加 --ensure-daemon 自动拉起守护进程）
focusstage launch --ensure-daemon foot
focusstage launch --ensure-daemon --kind x11 xterm
focusstage launch --ensure-daemon --kind chromium chromium

# 通过 FocusStage 路由链接或查看当前 Linux 系统设置
focusstage open --ensure-daemon https://example.com
focusstage settings get

# Linux MCP 客户端入口（stdio，与 focusstaged 使用同一 Linux 用户）
/usr/local/bin/focusstage mcp
```

安装器会自动将 `focusstage` 注册到 Windows 侧和 WSL 侧的用户级全局配置（Claude Code 的 `~/.claude.json` 与 Codex 的 `~/.codex/config.toml`），安装完成后新开会话即可在任意目录使用。如需手动接入其他 MCP 客户端，可按运行环境选择对应命令：

- **Linux / WSL 侧客户端**（与 `focusstaged` 同属同一 WSL 发行版与用户）：
  ```json
  {
    "mcpServers": {
      "focusstage": {
        "type": "stdio",
        "command": "/usr/local/bin/focusstage",
        "args": ["mcp"]
      }
    }
  }
  ```
- **Windows 侧客户端**（按需替换 `Ubuntu` 为实际发行版名）：
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

- `stage_launch` 默认以**后台隐藏模式**启动应用（`show_on_desktop` 默认为 `false`，需要直接显示到桌面时可显式传入 `true`），并默认等待窗口出图，最多 8 秒（可用 `wait_ready_ms` 指定 1–20000 毫秒），就绪时附上同一窗口帧的原图。`observe: false` 只跳过截图。向已有 Stage 发送打开参数时，等待新窗口或活动窗口的画面更新。显式要求新窗口时，只接受新映射的窗口。
- `stage_acquire` 默认保持 Stage 当前的显隐状态（`hide` 默认为 `false`）；若需将桌面窗口收入后台操作，可传 `hide: true`。当窗口留在桌面并由 AI 操控（`Desktop + Agent`）时，Win32 宿主会自动叠加 AI 接管覆盖层与虚拟指针动画。
- `ready` 表示窗口已有可绘制的帧，不代表应用内容已加载完成。如果首张截图仍是空白内容区，可以用 `stage_observe` 获取新一帧。
- 等待超时返回 `timeout` 并保留 Stage，已知的启动进程退出状态会放在 `launcher_exit`。确认启动失败时返回 `launch_failed`。如果窗口已就绪但截图发生渲染错误，结果保留 `ready` 和 Stage ID，并在 `observe_message` 中说明错误。
- `stage_observe` 可只传 `stage_id`，不会获取租约或更改窗口显隐。`stage_act` 仍需租约；只想发送输入时可用 `observe: false` 跳过截图和默认等待。需要等待时可显式传 `settle: true`，并用 `settle_ms` 覆盖默认的 600 ms 上限。
- `stage_drop_files` 需要有效租约和 `paths`（Windows 盘符路径或 WSL 绝对路径）。可同时传 `x`、`y` 指定窗口内逻辑坐标，否则使用当前 Stage 指针位置。成功表示已发送拖放，不保证应用接受文件。

**汉字备用字形**：如果 WSL 使用 `C.UTF-8`，而中文菜单中的个别汉字看起来像日文字形，可在 Linux 设置的「汉字备用字形」里选择简体中文，或运行 `focusstage settings set-han-fallback zh-cn`。也支持 `zh-tw`、`ja`；用 `system` 恢复默认。重新打开应用后生效。此选项只给新启动的普通 Stage 提供 Fontconfig 备用字体，不修改 Ubuntu 全局配置、应用界面语言或缩放；已指定的地区语言和 Fontconfig 环境优先，自带字体或不使用 Fontconfig 的应用不一定受影响。

**WSL2 图形渲染与 D3D12 显卡选择**：WSL2 检测到 `/dev/dxg` 时默认向 Stage 注入 `GALLIUM_DRIVER=d3d12`。在双显卡设备或遇到特定显卡驱动兼容问题时，可在 Linux 设置中切换「WSL2 图形渲染后端」（`auto` / `llvmpipe`）和「D3D12 显卡适配器」（映射到 `MESA_D3D12_DEFAULT_ADAPTER_NAME`），或使用 CLI：`focusstage settings set-graphics-driver <auto|d3d12|llvmpipe>`、`focusstage settings set-d3d12-adapter <auto|显卡名称>`。重新打开应用后生效；显式环境变量与单 Stage `launch_env` 覆盖优先。

### 3. 运行 Windows 11 启动器与托盘（`FocusStage.exe`）

源码位于 WSL 时，通过 `./scripts/install.sh --skip-rust-build` 复用已有的 Rust 构建产物。源码位于 Windows 磁盘时也可以直接调试启动器：

```cmd
dotnet run --project tray/FocusStageTray.csproj -c Release
```

默认全局接管热键为 `Ctrl+Alt+Space`，可立即作废所有 AI 租约并将后台 Stage 恢复至桌面。在 AI 桌面协同模式下（`Desktop + Agent`），窗口还会显示呼吸内发光边框、实时虚拟指针动效与顶部居中接管胶囊，直接点击胶囊上的「接管」按钮也可立即夺回控制权。

安装器会在所选 WSL 发行版中登记 Windows 宿主。`focusstaged` 启动时连接原生 Win32 宿主，宿主不可用或显式传入 `--headless` 时进入离屏模式。可用 `focusstaged --native-host <WSL 中的 FocusStage.exe 路径>` 测试开发版本。`focusstage --json ping` 的 `desktop_active` 表示原生宿主是否已连接。

原生窗口基于 Direct2D 渲染并支持大帧（≥ 256 KiB）可选 LZ4 压缩传输；WSL2 在检测到 `/dev/dxg` 时默认让 Chromium / Electron 走内置 XWayland + Mesa D3D12 硬件合成，保留 Linux 应用自己的标题控件（支持 Wayland 客户端标题栏拖动与缩放，新窗口默认居中），不叠加 Windows 标题栏。每个 Stage 可以有多个独立窗口，任务栏按 Stage 分组；关闭宿主窗口只向对应的 Linux 窗口发关闭请求，在 **舞台中心 → 运行舞台**（或托盘菜单）中「关闭」仍是结束整个 Stage。跨显示器移动时，支持 `wp_fractional_scale_v1` 的 Wayland 客户端会收到该窗口的 DPI 更新；X11 客户端仍由 XWayland 路径缩放。托盘的「Quick Launch Stage」会按当前已安装的 Linux 浏览器动态生成快捷项，并支持保留带引号参数的自定义命令。

启动器的 **舞台中心 → Linux 设置** 可选择默认浏览器和终端、同步用户级协议关联。Linux 应用通过 `xdg-open`、`gio open` 或 `focusstage open --ensure-daemon <URI>` 打开链接时，会复用已有的 Human Stage，或者启动目标应用。目标 Stage 被 Agent 租用、冻结或无法唯一确定时会返回错误。Windows 默认浏览器选项只接收 HTTP(S)。需要处理 `vscode://` 等 Linux 应用回调时，请使用 Linux 浏览器。

同一页面的 **过滤常见 WSL 标识** 默认关闭。开启后，FocusStage 从新启动的普通应用及其子进程的环境中移除 `WSL_DISTRO_NAME` 和 `WSL_INTEROP`，并为支持兼容库的应用过滤 `uname`、`/proc/version` 和 `/proc/sys/kernel/osrelease` 中的 WSL 内核标识。依赖这些标识的应用可以使用 Linux 桌面行为，例如用默认 Linux 文件管理器打开目录。过滤仅作用于应用进程，系统文件关联、全局 WSL 配置、`PATH` 和互操作文件保留原样。Snap 应用只应用环境变量过滤。已运行的应用需要退出后重开。

Chromium、Electron 和 Gecko 使用各自的 WSL 用户配置目录，关闭并重开后可以从原目录读取设置和 Cookie。再次启动已经由 FocusStage 托管的同款单实例应用时，会复用已有的 Human Stage。显式传入的 profile 参数会保留。

安装器提供 GNOME Keyring。VS Code 使用 Linux Secret Service 保存加密凭据时，首次使用可能提示创建带密码的密钥环，WSL 重启后可能需要重新解锁。若 VS Code 的 `argv.json` 显式设置了 `"password-store": "basic"`，VS Code 会按该设置选择存储后端。

---

## 目录结构

```text
FocusStage/
├── README.md                 # 项目概述与快速开始
├── DESIGN.md                 # 架构设计与收敛路线
├── CHANGELOG.md              # 版本变更记录
├── CONTRIBUTING.md           # 参与贡献与本地开发指南
├── SECURITY.md               # 安全策略与漏洞反馈方式
├── LICENSE                   # MIT 开源许可协议
├── .github/workflows/        # GitHub Actions（ci.yml 与 release.yml）
├── compositor/               # [WSL2] focusstaged 与 focusstage-tun：Rust + Smithay 合成器、Router、Stage Manager、SessionEnv、XDG/图标解析、DisplaySink、透明代理桥
├── mcp/                      # [Linux] focusstage：CLI、应用卸载与 MCP stdio 服务
├── tray/                     # [Win11] .NET 8 + WebView2 启动器（应用桌面 + 舞台中心）、托盘热键、Windows Hello 凭据托管与 Direct2D 原生 Win32 宿主
├── third_party/smithay/      # 内嵌 Smithay 合成器库（MIT 许可）
├── packaging/                # Inno Setup 终端用户安装包配置（FocusStage.iss）
└── scripts/                  # 开发部署、Release 独立安装、卸载、Polkit 规则与 MCP/启动辅助脚本
```

---

## 常见问题（FAQ）

- **双击安装器时 Windows SmartScreen 提示“已保护你的电脑”？**
  个人开源项目尚未购买商业 EV 代码签名证书，首次下载的 `.exe` 安装包可能触发 SmartScreen 警告。可先核对 Release 页面附带的 `SHA256SUMS.txt` 校验值（在 PowerShell 运行 `Get-FileHash FocusStage-Setup-*.exe -Algorithm SHA256`），确认一致后点击「更多信息 → 仍要运行」；也可以从源码自行构建。
- **双显卡笔记本出现黑屏、卡顿或渲染异常？**
  打开启动器 **舞台中心 → Linux 系统设置 → Linux 应用兼容性**，可在「D3D12 显卡适配器」中显式指定宿主显卡，或将「WSL2 图形渲染后端」切换为软件渲染（`llvmpipe`），重新打开对应 Linux 应用即生效。
- **中文界面出现部分日文字形？**
  在 **舞台中心 → Linux 系统设置** 中将「汉字备用字形」设为简体中文（`zh-cn`）或繁体中文（`zh-tw`），重新打开应用生效。
- **如何在 WSL2 中安装 Linux 桌面应用？**
  在 Ubuntu 终端直接通过 `sudo apt install ...`、`.deb` 安装包、Flatpak 或 Snap 安装即可，FocusStage 会自动识别标准 `.desktop` 入口并同步到启动器和 Windows 开始菜单。

---

## 运行环境

- **终端用户运行环境**：Windows 10 / 11（64 位）+ WSL2 + Ubuntu（推荐 Ubuntu 22.04 / 24.04 / 26.04 LTS；Release 安装器自动补齐缺失的 Linux 运行库与桌面组件，无需预装 Rust 或 `.NET SDK`）
- **开发者构建环境**：Windows 11 具备 `.NET 8 SDK`、`Git`（打包 `.exe` 安装器可选 Inno Setup 6）；WSL2 Ubuntu 具备 `Rust` 工具链与 `libwayland-dev` / `libpixman-1-dev` / `libxkbcommon-dev` / `pkg-config`
- **架构与收敛细节**：见 [DESIGN.md](DESIGN.md)

---

## 参与贡献与许可协议

- 欢迎提交 [Issue](https://github.com/Charlie929-dot/FocusStage/issues) 或 Pull Request，开发与测试指引见 [CONTRIBUTING.md](CONTRIBUTING.md)。
- 如发现安全漏洞，请遵循 [SECURITY.md](SECURITY.md) 私密上报。
- 本项目基于 [MIT License](LICENSE) 开源；内嵌的 [Smithay](https://github.com/Smithay/smithay) 合成器库同样遵循 MIT 许可（见 [third_party/smithay/LICENSE.txt](third_party/smithay/LICENSE.txt)）。
