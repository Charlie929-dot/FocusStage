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

#### 方式 A：直接安装预编译发行版（终端用户）

若不想在本地安装 Rust 与 `.NET SDK`，可从 GitHub Releases 下载 `FocusStage-Setup-<tag>.exe` 安装包（或解压 `FocusStage-<tag>-win-x64.zip` 后运行 `install-release.ps1`）：

- 自动检测本机 WSL2 与 Ubuntu 发行版（不写死发行版名称，缺失时可引导安装 `Ubuntu-24.04`，多发行版时支持选择）；
- 通过 `apt-get install --no-upgrade` 仅补齐缺失的桌面组件与运行库（`libpixman-1-0`、`libwayland-server0`、`libxkbcommon0`、`libffi8` 等）；
- 部署预编译 `focusstaged` / `focusstage` 与自包含 `FocusStage.exe` 到 `%LOCALAPPDATA%\FocusStage`（自动旁路 MSIX 文件系统虚拟化），登记宿主路径、同步快捷方式并注册 Claude Code MCP，支持在 Windows「应用和功能」中直接卸载。

#### 方式 B：从源码一键构建部署（开发者）

在 Windows PowerShell 中运行开发安装器，会自动构建并安装 WSL2 端（`focusstaged` / `focusstage` / `focusstage-launch` / `focusstage-open`）及所需 Linux 桌面组件（`gnome-keyring`、`xdg-desktop-portal-gtk`、`xwayland`、`fonts-noto-cjk` 等），发布 `FocusStage.exe` 到 `%LOCALAPPDATA%\FocusStage` 并校验登记原生窗口宿主，初始化 Linux 协议关联，同时同步所有 Linux 应用到 Windows 开始菜单及桌面入口：

```powershell
# 源码位于当前 Windows 仓库时
powershell -ExecutionPolicy Bypass -File scripts/install.ps1

# 推荐：源码位于 WSL 原生文件系统（如 ~/src/FocusStage）时，直接指定 -WslRepo
powershell -ExecutionPolicy Bypass -File scripts/install.ps1 -WslRepo ~/src/FocusStage
```

若当前就在 WSL 终端中，也可以直接运行封装脚本一键构建并默认部署到 Windows：

```bash
./scripts/deploy.sh
# 若在 FocusStage 托管的窗口（如 IDE）中执行，保留当前窗口宿主：
./scripts/deploy.sh --keep-window-host
```

如果直接在 Windows 文件资源管理器中从 `\\wsl.localhost\...` 打开安装脚本，请双击运行 `scripts/install.cmd`（会自动绕过 PowerShell UNC 执行策略限制并在结束时保留窗口提示）。

安装完成后：
- 双击桌面或开始菜单中的 **FocusStage** 打开统一应用启动器。启动器优先使用 Windows Hello，也可通过 Ubuntu 管理员密码验证。验证后会进入 WSL 默认账号的桌面会话。`root` 直接使用现有权限，有复用授权的普通管理员可在启动器打开期间免重复执行 `sudo` 和标准 Polkit 验证。最小化会保留授权，关闭到托盘会撤销。已启动的提权进程会继续运行。
- 安装器为 WSL 默认的非 `root` 管理员接入 PAM 和 Polkit，并关闭该账号的 `sudo` 密码缓存。若 WSL 默认账号是 `root`，Linux 密码备选会使用此前配置的非 `root` 管理员。没有这个账号时可用 Windows Hello。卸载 FocusStage 时会移除授权设置。
- 开始菜单 `Programs\FocusStage\` 下会自动生成全部 Linux 应用的独立快捷方式（按 Linux 图标主题解析并带图标），可右键“固定到开始屏幕”或“固定到任务栏”。
- 安装器会把 `focusstage` 注册到 Windows 用户的 Claude Code MCP 配置 `~/.claude.json`，使用 `-Distro` 指定的 WSL 发行版，默认是 `Ubuntu`（若直接从 `\\wsl.localhost\<Distro>\...` 运行安装器也会自动识别发行版）。MCP 注册需要该发行版中的 `python3`。
- 当仓库位于 WSL UNC 路径或传入 `-WslRepo` 时，脚本会自动将 `tray/` 镜像到 Windows 临时 NTFS 目录编译再部署，绕过 9P 慢速与 UNC 限制；常用开发开关：`-WslRepo`、`-Distro`、`-SkipRustBuild`、`-SkipTrayBuild`、`-NoLaunch`、`-SkipMcpConfig`。
- 运行 `powershell -ExecutionPolicy Bypass -File scripts/uninstall.ps1` 会自动读取部署时记录的 `distro.txt`（或使用 `-Distro`），清理 `~/.claude.json` MCP 条目、移除协议关联、授权配置与快捷方式并删除安装目录（Inno Setup 卸载器传入 `-KeepInstallDir` 后由安装器自身清理目录）。

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

仓库自带 Linux 侧的项目级 MCP 配置：Claude Code 使用 `.mcp.json`，Codex（包括 T3 Code 中的 Codex）使用 `.codex/config.toml`。当 Harness 和 `focusstaged` 在同一 Linux 环境运行（包括同一个 WSL 发行版），Harness 可以直接调用 `focusstage`。从 Windows 安装 FocusStage 时，安装器会将这些 Linux 程序部署到 WSL，无需在 WSL 内另行安装。

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

在 Linux 侧的 Harness 中信任此项目并开启新会话即可加载配置；Claude Code 还需允许项目 MCP。如果 Harness 本身运行在 Windows，则需要通过 `wsl.exe -d Ubuntu -e /usr/local/bin/focusstage mcp` 转接（按需替换发行版名）。Windows 安装器默认只为 Windows 侧的 Claude Code 注册该转接，不会自动注册 Windows 侧的 Codex。

打开已安装的 GUI 应用时，先用 `stage_list_apps` 查目录（可传 `query`），再把返回的 `id` 作为 `stage_launch` 的 `app_id`。也可以传唯一的 `app_name`。目录启动复用启动器登记的命令和参数；`app` 仍可用于显式运行命令。

- `stage_launch` 默认以**后台隐藏模式**启动应用（`show_on_desktop` 默认为 `false`，需要直接显示到桌面时可显式传入 `true`），并默认等待窗口出图，最多 8 秒（可用 `wait_ready_ms` 指定 1–20000 毫秒），就绪时附上同一窗口帧的原图。`observe: false` 只跳过截图。向已有 Stage 发送打开参数时，等待新窗口或活动窗口的画面更新。显式要求新窗口时，只接受新映射的窗口。
- `stage_acquire` 默认保持 Stage 当前的显隐状态（`hide` 默认为 `false`）；若需将桌面窗口收入后台操作，可传 `hide: true`。当窗口留在桌面并由 AI 操控（`Desktop + Agent`）时，Win32 宿主会自动叠加 AI 接管覆盖层与虚拟指针动画。
- `ready` 表示窗口已有可绘制的帧，不代表应用内容已加载完成。如果首张截图仍是空白内容区，可以用 `stage_observe` 获取新一帧。
- 等待超时返回 `timeout` 并保留 Stage，已知的启动进程退出状态会放在 `launcher_exit`。确认启动失败时返回 `launch_failed`。如果窗口已就绪但截图发生渲染错误，结果保留 `ready` 和 Stage ID，并在 `observe_message` 中说明错误。
- `stage_observe` 可只传 `stage_id`，不会获取租约或更改窗口显隐。`stage_act` 仍需租约；只想发送输入时可用 `observe: false` 跳过截图和默认等待。需要等待时可显式传 `settle: true`，并用 `settle_ms` 覆盖默认的 600 ms 上限。
- `stage_drop_files` 需要有效租约和 `paths`（Windows 盘符路径或 WSL 绝对路径）。可同时传 `x`、`y` 指定窗口内逻辑坐标，否则使用当前 Stage 指针位置。成功表示已发送拖放，不保证应用接受文件。

### 3. 运行 Windows 11 启动器与托盘（`FocusStage.exe`）

```powershell
# 源码位于 Windows 本地磁盘时可直接运行调试；若源码在 WSL 文件系统建议通过 scripts/install.ps1 -WslRepo ~/src/FocusStage -SkipRustBuild 部署
dotnet run --project tray/FocusStageTray.csproj -c Release
```

默认全局接管热键为 `Ctrl+Alt+Space`，可立即作废所有 AI 租约并将后台 Stage 恢复至桌面。在 AI 桌面协同模式下（`Desktop + Agent`），窗口还会显示呼吸内发光边框、实时虚拟指针动效与顶部居中接管胶囊，直接点击胶囊上的「接管」按钮也可立即夺回控制权。

安装器会在所选 WSL 发行版中登记 Windows 宿主。`focusstaged` 启动时连接原生 Win32 宿主，宿主不可用或显式传入 `--headless` 时进入离屏模式。可用 `focusstaged --native-host <WSL 中的 FocusStage.exe 路径>` 测试开发版本。`focusstage --json ping` 的 `desktop_active` 表示原生宿主是否已连接。

原生窗口基于 Direct2D 渲染并支持大帧（≥ 256 KiB）可选 LZ4 压缩传输；WSL2 在检测到 `/dev/dxg` 时默认让 Chromium / Electron 走内置 XWayland + Mesa D3D12 硬件合成，保留 Linux 应用自己的标题控件（支持 Wayland 客户端标题栏拖动与缩放，新窗口默认居中），不叠加 Windows 标题栏。每个 Stage 可以有多个独立窗口，任务栏按 Stage 分组；关闭宿主窗口只向对应的 Linux 窗口发关闭请求，在 **舞台中心 → 运行舞台**（或托盘菜单）中「关闭」仍是结束整个 Stage。跨显示器移动时，支持 `wp_fractional_scale_v1` 的 Wayland 客户端会收到该窗口的 DPI 更新；X11 客户端仍由 XWayland 路径缩放。托盘的「Quick Launch Stage」会按当前已安装的 Linux 浏览器动态生成快捷项，并支持保留带引号参数的自定义命令。

启动器的 **舞台中心 → Linux 设置** 可选择默认浏览器和终端、同步用户级协议关联。Linux 应用通过 `xdg-open`、`gio open` 或 `focusstage open --ensure-daemon <URI>` 打开链接时，会复用已有的 Human Stage，或者启动目标应用。目标 Stage 被 Agent 租用、冻结或无法唯一确定时会返回错误。Windows 默认浏览器选项只接收 HTTP(S)。需要处理 `vscode://` 等 Linux 应用回调时，请使用 Linux 浏览器。

同一页面的 **过滤常见 WSL 标识** 默认关闭。开启后，FocusStage 为新启动的非提权应用过滤 WSL 环境变量和 PATH 中的 Windows 路径，并为普通动态链接应用过滤部分 `uname` 和 `/proc` 探针。这可让依赖这些标识的应用选择 Linux 桌面行为，例如用默认 Linux 文件管理器打开目录。它不修改系统文件关联或全局 WSL 配置，也不保证覆盖所有探测方式；Snap 不加载兼容库。已运行的应用需要退出后重开，依赖 Windows 互操作的功能可能受影响。

Chromium、Electron 和 Gecko 使用各自的 WSL 用户配置目录，关闭并重开后可以从原目录读取设置和 Cookie。再次启动已经由 FocusStage 托管的同款单实例应用时，会复用已有的 Human Stage。显式传入的 profile 参数会保留。

安装器提供 GNOME Keyring。VS Code 使用 Linux Secret Service 保存加密凭据时，首次使用可能提示创建带密码的密钥环，WSL 重启后可能需要重新解锁。若 VS Code 的 `argv.json` 显式设置了 `"password-store": "basic"`，VS Code 会按该设置选择存储后端。

---

## 目录结构

```text
FocusStage/
├── README.md                 # 项目概述与快速开始
├── DESIGN.md                 # 架构设计与收敛路线
├── .mcp.json                 # 项目级 MCP 配置示例
├── .github/workflows/        # GitHub Actions CI/CD（release.yml 发版流水线）
├── compositor/               # [WSL2] focusstaged：Rust + Smithay 合成器、Router、Stage Manager、SessionEnv、XDG/图标解析、DisplaySink
├── mcp/                      # [Linux] focusstage：CLI、应用卸载与 MCP stdio 服务
├── tray/                     # [Win11] .NET 8 + WebView2 启动器（应用桌面 + 舞台中心）、托盘热键与 Direct2D 原生 Win32 宿主
├── packaging/                # Inno Setup 终端用户安装包配置（FocusStage.iss）
└── scripts/                  # 开发部署、Release 独立安装、卸载、PAM/Polkit 授权与 MCP/启动辅助脚本
```

---

## 运行环境

- **终端用户运行环境**：Windows 10 / 11（64 位）+ WSL2 + Ubuntu（推荐 Ubuntu 22.04 / 24.04 / 26.04 LTS；Release 安装器自动补齐缺失的 Linux 运行库与桌面组件，无需预装 Rust 或 `.NET SDK`）
- **开发者构建环境**：Windows 11 具备 `.NET 8 SDK`、`Git`（打包 `.exe` 安装器可选 Inno Setup 6）；WSL2 Ubuntu 具备 `Rust` 工具链与 `libwayland-dev` / `libpixman-1-dev` / `libxkbcommon-dev` / `pkg-config`
- **架构与收敛细节**：见 [DESIGN.md](DESIGN.md)
