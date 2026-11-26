# FocusStage 架构设计文档

> 本文档描述 FocusStage 的产品定位、核心模型、模块划分、AI 控制接口与收敛路线。

---

## 1. 目标、定位与问题根源

### 1.1 要解决的问题

传统桌面系统只有**一个全局鼠标、一个前台焦点、一个剪贴板**。AI Agent 想操作某个窗口，就必须抢走用户的鼠标和焦点——人和 AI 只能轮流用电脑。

FocusStage 要实现的体验：

1. **存在方式像原生安装软件**：Linux 应用通过图形化启动器、开始菜单和任务栏便捷启动与管理，以无冗余标题栏、原生任务栏分组的 Win11 窗口出现在桌面上，文件直接读写真实工作目录。
2. **随时交给 AI**：一键把某个窗口「收入后台」，它在后台继续正常运行，AI 独立操作它；用户的鼠标、正在打字的窗口和剪贴板**不受任何影响**。
3. **多窗口真并发**：AI 1 操作窗口 A、AI 2 操作窗口 B、用户自己操作窗口 C，三者互不抢焦点。
4. **随时收回**：AI 做完或用户按下接管热键，窗口回到原来的位置和大小，画面与登录态完全连续。

### 1.2 产品定位：把边界划清楚

- **“像 Windows 应用”指的是存在方式，而不是软件体验**：
  - 追求的是**启动便捷、管理直观、窗口存在自然**——像直接装在 Windows 里的软件一样能从启动器/开始菜单点开、分组停靠在任务栏、没有 WSLg 的丑标题栏与多余外壳。
  - **不追求把 Linux 软件内部体验改造成 Windows 软件**，保留 Linux 应用原本的界面、标题控件与交互习惯。
- **独特价值做成启动器内的“舞台中心”系统视图**：
  - 「应用桌面」视图负责帮助用户方便地发现、整理、启动与卸载应用；
  - FocusStage 区别于普通启动器的核心价值——**运行中舞台状态观测、前后台切换、AI 租约监控、守护进程管理、Linux 默认应用与外观设置**——独立承载在启动器顶层视图「舞台中心」内，而不是塞在右键托盘菜单里，也不让应用桌面膨胀成复杂的全能应用商店。

### 1.3 为什么是「WSL2 + 自研 Wayland 合成器」

| 路线 | 结论 | 原因 |
| :--- | :--- | :--- |
| Win32 消息 / UIA / DLL 注入 | 放弃 | Chrome/Electron/自绘客户端不理会后台 `PostMessage`，UIA 覆盖不全，注入易触发风控或崩溃 |
| Windows 虚拟机 | 放弃 | VM 内部依然是单焦点，多个窗口不能并发；镜像与内存开销重 |
| 直接用 WSLg | 不够 | WSLg 的 Weston 把所有窗口放在同一套焦点里，无法按窗口隔离输入与离屏托管；其 RAIL 强制标题栏与任务栏外观也不可控 |
| **自研 Wayland 合成器（跑在 WSL2）** | **采用** | 合成器是用户态程序，**每个输入事件送给谁完全由我们决定**；配合原生 Win32 宿主即可实现干净的窗口存在方式，核心不绑定 Windows，远期可换 DRM/KMS 后端独立开机 |

### 1.4 设计原则

1. **只自研不可替代的部分**：多焦点隔离、离屏托管与原生无标题栏 Win32 宿主（`NativeHost`）必须自己掌控；Linux 应用安装与分发外包给系统自带的应用中心 / `apt` 等标准机制，不自建包管理器。
2. **单一显示后端**：`NativeSink` 与 `NativeHost` 统一管理窗口状态、帧提交和输入路由。
3. **通用机制优先，不做逐应用打补丁**：进程归属、后台脱离存活、失败日志观测都通过通用机制解决，而不是针对每个软件堆砌特判。
4. **Windows 侧只做交互与呈现壳，核心逻辑全在 WSL 内核**：启动器、内置系统软件和窗口宿主负责入口、展示、热键与原生窗口呈现；所有 Stage 状态、路由与租约逻辑集中在 `focusstaged`。

---

## 2. 核心模型

### 2.1 Stage：隔离单位是应用实例，不是单个窗口

**Stage（舞台）= 一个被托管的应用实例（进程树）**，包含它的所有顶层窗口、弹窗、子 surface。

- 应用必须通过 FocusStage 启动（`stage.launch`），不能中途接管已经在 Win11 上运行的 `.exe`。
- 多焦点隔离服务于跨应用并发。Chromium、Electron 和 Gecko 默认使用各自的 WSL 用户配置，关闭再启动可以保留设置和 Cookie。安装器提供 `gnome-keyring`，守护进程在 `XDG_CURRENT_DESKTOP` 未设置时使用 `GNOME`，让应用能选择 Linux Secret Service。密钥环由用户会话管理，应用读取加密凭据时需要可用且已解锁的密钥环。主动指定的 profile 参数由应用处理。
- 同款单实例应用的再次启动、链接及 URI 回调由 `focusstaged` 转交给已有的 Human Stage。被 Agent 租用、已冻结或归属不明确时返回错误。已有的显式 profile 随转交沿用。已经通过其他合成器启动的实例需先退出，再由 FocusStage 启动。
- X11 应用每个 Stage 配一个独立的 rootless `Xwayland` 实例。
- 同一应用内部的多个顶层窗口、对话框、菜单、弹窗都属于同一个 Stage，由 Stage 内部维护活跃窗口与窗口间切换；在 Windows 任务栏中按 Stage 分组呈现。

> 为什么不按窗口隔离？同一进程的多个窗口共享一个 Wayland 连接，合成器无法让它们"同时都是前台"而不让客户端察觉矛盾。以应用实例为单位，矛盾就不存在了。

### 2.2 虚拟焦点：所有 Stage 永远处于前台

**不使用多 `wl_seat`**。原因：多 Seat 依赖客户端的支持，Chromium/Electron 支持有限，XWayland 则完全不支持多 Seat。

替代方案：

- 合成器只暴露**一个 `wl_seat`**；
- 对**每一个** Stage，合成器维护独立的虚拟焦点，使其在需要离屏或被 AI 操作时始终认为自己处于激活/前台状态；
- 由于客户端之间互相看不到对方的 surface，这种"多个前台"不会被察觉；
- 每个输入事件最终送给哪个 Stage，由合成器内部的 **Router** 根据路由表决定。

修饰键状态（Ctrl/Shift 等）、键盘重复、指针位置都**按 Stage 分别维护**，AI 在 Stage A 按住 Ctrl 不会影响用户在 Stage B 里打字。

### 2.3 状态：两个互相独立的维度

每个 Stage 的状态由两个维度组合而成：

```text
显示位置 (display):   Desktop  |  Hidden
输入归属 (input):     Human    |  Agent(lease_id)  |  Frozen
```

| 组合 | 含义 |
| :--- | :--- |
| Desktop + Human | 日常使用：窗口在 Win11 桌面上，用户正常操作 |
| Hidden + Agent | 收入后台：窗口从桌面消失，AI 独立操作 |
| Desktop + Agent | 当面操作：AI 在用户眼前操作，用户只能看，按接管热键可立即抢回 |
| Hidden + Frozen | 挂起：暂时搁置，谁都不能输入 |
| Desktop + Frozen | 只读展示：可以看，不能操作 |

**状态切换只修改 Router 的路由表和显示开关，应用本身不需要重启或重连**——它始终在运行、始终在渲染、画面与登录态天然连续。

切换规则：

- **人永远优先**：任何状态下，用户按接管热键（或点击接管按钮）都会立刻把输入归属改为 Human，并作废当前 AI 租约。
- 收入后台时记录各窗口在桌面上的位置与大小（`SavedRect`），恢复时按 `SavedRect` 放回原处。
- 输入归属为 Agent 时，来自用户的输入一律丢弃（接管热键除外），防止人机同时点击。

---

## 3. 总体架构

```text
                        Windows 11
 ┌──────────────────────────────────────────────────────────────┐
 │  AI Agent (Claude Code / Cursor / 脚本)                       │
 │      │ MCP over stdio                                         │
 │      ▼                                                        │
 │  wsl.exe -e focusstage mcp   ──────────────┐                  │
 │                                            │                  │
 │  FocusStage.exe (.NET 8 + WebView2, 交互壳 + Direct2D 原生宿主) │
 │   · 应用桌面：应用网格 / 文件夹 / 快捷栏 / 快捷方式同步 / 右键卸载 │
 │   · 舞台中心：运行舞台显隐与接管 / 守护进程与热键 / Linux 设置 / 外观 │
 │   · 托盘后台常驻：全局接管热键 / 动态浏览器快速启动 / 剪贴板同步  │
 │   · --window-host：Direct2D 原生宿主，支持 LZ4 帧、AI 接管覆盖层与原生 Polkit 提权弹窗 │
 └───────────────────────▲────────────────────┼──────────────────┘
                         │ BGRA/LZ4 帧 & AI 覆盖层 / 键鼠、窗口状态、提权与 DPI（stdio）
                        WSL2                  ▼
 ┌──────────────────────────────────────────────────────────────┐
 │  focusstaged  (Rust + Smithay, 单进程守护)                     │
 │                                                              │
 │   Stage Manager   通用应用启动 / 进程树归属 / XWayland+D3D12   │
 │   SessionEnv      WSL 桌面环境变量统一解析 & 原生 Polkit 代理   │
 │   Router          虚拟焦点 · 输入归属 · AI 租约 · 覆盖层指针状态 │
 │   Offscreen Core  虚拟 wl_output · frame 节拍 · 按需截图       │
 │   Clipboard       每个 Stage 一份选区，按策略与 Windows 互通    │
 │   A11y            AT-SPI 无障碍树（按 PID 归属到 Stage）        │
 │   XDG & Icons     .desktop 发现 · 图标主题解析 · URI/协议路由  │
 │   Control API     Unix socket，供 MCP / 启动器 / CLI 使用     │
 │                                                              │
 │   DisplaySink 接口 ─── NativeSink  原生 Win32 窗口             │
 │                                                              │
 │        WAYLAND_DISPLAY=focusstage-0  /  每 Stage 一个 Xwayland │
 │              ▼                ▼                ▼              │
 │      [Wayland / Gecko]   [Chromium/Electron]  [X11 应用]       │
 │                                                              │
 │   文件直通：/mnt/c/Users/<user>/...（与 Win11 同一份文件）       │
 └──────────────────────────────────────────────────────────────┘
```

**业务逻辑全部集中在 WSL 内的 `focusstaged` 一个进程里**。Windows 侧（`FocusStage.exe`）承担交互壳与显示宿主：提供便捷的应用启动/管理入口、内置系统信息与控制界面、全局接管热键以及原生 Win32 窗口渲染。

---

## 4. 模块设计

### 4.1 Stage Manager

- 根据应用描述（命令、参数、类型 `Wayland` / `Chromium` / `Electron` / `Gecko` / `X11`）启动进程，经 `SessionEnv` 统一解析并注入 `WAYLAND_DISPLAY`、`DISPLAY`、`FOCUSSTAGE_STAGE_ID`、`FOCUSSTAGE_LAUNCH_TOKEN` 等桌面环境（WSL 环境下同时设置 `DONT_PROMPT_WSL_INSTALL=1`）；
- **WSL2 图形加速默认路径**：在检测到 `/dev/dxg` 的 WSL2 环境中默认设置 `GALLIUM_DRIVER=d3d12`（允许显式环境变量覆盖）；Chromium 与 Electron 应用默认走合成器内置 rootless `Xwayland`（保留显式 `--ozone-platform` 参数）以启用 ANGLE/D3D12 硬件合成，避免 Wayland 软件回退造成的掉帧；
- X11 Stage 以及 Snap 包装的 Chromium/Electron 应用：先拉起专属的 rootless `Xwayland`（合成器内置 XWM），再在对应 `DISPLAY` 上启动应用；`.AppImage` 文件会自动缓存到 `~/.cache/focusstage/appimages` 并以 `APPIMAGE_EXTRACT_AND_RUN=1` 运行；
- **通用进程归属与生命周期（已落地）**：
  - 不假设“初始启动命令就是常驻 GUI 主进程”——Snap、Electron、IDE 包装脚本常在拉起后台 GUI 后自行退出；
  - 进程归属结合唯一启动令牌（`FOCUSSTAGE_LAUNCH_TOKEN`，经 `/proc/<pid>/environ` 匹配）、Stage 标识环境与进程树信息，而不仅依赖初始 PID 的单层父子链；
  - 初始壳进程脱离退出时不误杀 Stage，结合存活窗口与持有同一令牌的进程统一回收；子进程 `stderr` 透出到守护进程日志（含 URI 回调的拉起单独静默 `stderr` 以防泄露凭据），并将已知的启动进程退出状态回传给 `stage_launch`；
- **单实例复用**：Chromium / Electron / Gecko 等单实例应用重复启动或收到协议回调时，由 `focusstaged` 校验启动环境与 profile 参数后转交给已有的 `Human` Stage（若目标被 Agent 租用、已冻结或匹配歧义则返回错误）。

### 4.2 Router（虚拟焦点与输入归属）

- 维护 `stage_id → { input_owner, lease, per-stage 键盘/指针状态 }`；
- 输入来源有三类：`DisplaySink`（`NativeSink`）转来的**人工输入**、MCP 转来的 **AI 输入**、启动器/热键发来的**接管指令**；
- 每个事件按"目标 Stage 当前的输入归属"校验后才投递，不符合的直接丢弃；
- 接管指令是硬中断：立即切换归属、作废租约、丢弃该 Stage 队列中尚未执行的 AI 动作；
- Stage 内部若有多个顶层窗口，键盘焦点跟随最近激活的顶层窗口或模态对话框，且当 Stage 处于 `Hidden` 或 `Agent` 态时保持虚拟激活不被宿主机窗口失焦事件破坏。

### 4.3 Offscreen Core（离屏托管与截图）

- 合成器创建一个**虚拟 `wl_output`**，所有 Stage 的 surface 均关联该 output，应用因此认为自己是"可见的"；
- **按需合成**：
  - `Hidden` 状态的 Stage 按固定节拍回复 `wl_surface.frame`，保证应用不降帧、不暂停；
  - 只有当 AI 调用 `observe`、系统监控需要缩略图或 `Desktop` 态需要推送显示帧时，才把主窗口 + 所有 `xdg_popup` + 子 surface 合成为以主窗口为原点的图像；
- 渲染器默认使用 **Pixman 软件渲染**（WSL2 无稳定 DRM render node，应用自身渲染不受影响）；
- 每次合成产生递增的 `frame_seq`，供 AI 接口做过期检测。

### 4.4 DisplaySink（把 Stage 显示到 Win11 桌面）

统一接口：窗口创建/更新/销毁、显隐切换、脏区推帧、输入事件与窗口状态回调。

**NativeSink（唯一桌面显示后端）**

- `focusstaged` 通过 WSL 互操作启动 Windows 侧 `FocusStage.exe --window-host`，以 stdio 二进制/JSON 协议传输像素帧（Direct2D 立即呈现；达到 256 KiB 且压缩收益明显的帧可选用 LZ4 压缩传输）与键鼠、窗口状态、DPI 及 AI 覆盖层事件；
- **AI 接管覆盖层（Desktop + Agent）**：当 Stage 保留在桌面并由 AI 租用操控时，原生宿主在 Direct2D 渲染层叠加呼吸内发光边框、顶部居中状态胶囊（展示 `agent_name` 与可直接点击的「接管 / CancelTakeover」按钮）及动态虚拟指针与点击/拖拽/滚动脉冲，既让用户清晰感知 AI 操作位置，又支持窗口内一键抢回控制权；
- 每个 Desktop 态顶层窗口对应一个独立的无边框 Win32 `HWND`：保留 Linux 应用自身的 CSD 标题栏控件（支持 Wayland 客户端标题栏拖动与缩放，新窗口默认居中），不叠加 Windows 标题栏，支持 Win11 原生圆角、任务栏按 Stage 分组（`AppUserModelID`）及同 Stage 多窗口；
- 宿主层统一处理 Windows 切窗修饰键同步，防止 `Alt+Tab` 等切窗操作向托管应用漏发 `Alt`；
- 关闭单个宿主窗口仅向对应 Linux 窗口发送关闭请求；收入后台即隐藏宿主窗口并保留 `SavedRect`，恢复时原位呈现；
- 跨显示器移动时，向支持 `wp_fractional_scale_v1` 的 Wayland 客户端同步该窗口的 DPI 更新；X11 客户端走 XWayland 缩放路径。

- 宿主不可用或显式指定 `--headless` 时，守护进程进入离屏模式，继续提供 Stage、截图和 MCP 输入。

**DrmSink（远期）**

- DRM/KMS + libinput 后端，直接接管物理显示器与键鼠，脱离 Windows 独立开机。

### 4.5 Clipboard

- 每个 Stage 持有**自己的一份选区**（clipboard + primary）；合成器在向某个 Stage 投递焦点时，只向它提供它自己的选区；
- 默认策略：
  - `Human` 状态的 Stage 与 Windows 剪贴板**双向同步**；
  - `Agent` 状态的 Stage **完全隔离**，AI 的复制粘贴不会污染 Windows 剪贴板，也不会串到别的 Stage；
- AI 可通过 MCP 显式读写自己 Stage 的剪贴板。

### 4.6 A11y（无障碍树）

- 读取 AT-SPI 会话总线，按 PID 把可访问对象归属到 Stage；
- 为 AI 提供结构化的界面信息（控件角色、名称、文字、边界框），与纯视觉 Set-of-Mark（SoM）打标互补；
- Chromium/Electron 在启动时按需开启 `--force-renderer-accessibility`。

### 4.7 文本输入

- **AI 输入文字**：不走输入法。优先通过 `zwp_text_input_v3` 直接 commit 字符串；不支持的应用（含 X11）回退为临时键位映射模拟按键；
- **用户输入中文**：
  - 当前：WSL 内通过 `Fcitx5` + `text-input-v3` 输入（需避免 Win32 IME 抢先拦截原生宿主按键）；
  - 后续演进：探索由 `NativeHost` 直接承接 Win32 IME 上屏文本并转成 `zwp_text_input_v3` preedit/commit，省去 WSL 内配置输入法的成本。

### 4.8 文件与外部集成

- **会话字体回退**：普通 Stage 的共同启动路径由 `SessionEnv` 解析应用覆盖值和继承的地区设置。默认完全使用 Ubuntu 的 Fontconfig；仅当用户选择了备用字形且没有明确的语言或字体配置时，向子进程传入包含系统规则的 `FONTCONFIG_FILE`。此设置不按应用名分支、不改系统 locale、不导入用户总线，也不作用于提权程序和持久服务。字形选择发生在应用侧，不在合成器或 Windows 宿主中重绘文字。
- 引导应用的默认目录（XDG User Dirs）指向 `/mnt/c/Users/<user>/` 下的桌面、下载、文档与图片目录；
- 已知限制：`/mnt/c` 走 9P 文件系统，大量小文件读写（如大仓库构建）明显变慢，对性能敏感的项目建议放在 WSL 原生文件系统中；
- 从资源管理器拖文件到宿主窗口（或 AI 调用 `stage_drop_files`）：路径 `C:\...` 转为 `/mnt/c/...`，经 `wl_data_device` 投递拖放；
- Wayland 应用发起拖放时，FocusStage 按鼠标落点在同窗口或跨宿主窗口间转发 Wayland 拖放；将带本地文件（`text/uri-list`）的拖拽移到 FocusStage 窗口之外时，交给 Windows 原生 OLE 拖放（仅复制 `CF_HDROP`），拖回宿主窗口则交还 Wayland 拖拽；
- **默认应用与协议关联（XDG / URI 路由）**：通过 `focusstage settings`（及启动器中的「舞台中心 → Linux 设置」）维护 `~/.config/focusstage/settings.json` 与用户级 `mimeapps.list` / handler `.desktop` 条目，把 `xdg-open` / `gio open` / `focusstage-open` 收到的 `http(s)`、自定义协议（如 `vscode://`）和本地文件交由 `focusstage open --ensure-daemon` 路由到对应 Stage；默认浏览器支持托管 Linux 浏览器或仅限 HTTP(S) 的 Windows 默认浏览器（`windows-host`）；
- **桌面会话、认证与图标主题**：`focusstaged` 与 `focusstage` 通过 `SessionEnv` 统一解析并向 `systemd --user` 同步桌面环境变量；`focusstaged` 内置原生 Polkit D-Bus 认证代理（`polkit.rs`），将 `pkexec` / 系统提权请求转交给 Windows 宿主弹出原生 Win32 提权框（`NativeElevationDialog`），不再依赖 `policykit-1-gnome`；`focusstage list-apps` / `sync-apps` 通过 Linux GTK 图标主题解析并缓存应用图标供启动器和 Windows 开始菜单复用。

### 4.9 Windows 启动器与内置舞台中心

`FocusStage.exe`（`tray/`）已由早期 GDI+ WinForms 重构为 **.NET 8 + WebView2（`tray/webui/`）交互壳**，严格保持为**展示与入口壳**，不承载合成器核心业务逻辑：

- **应用桌面（原生存在方式与整理）**：
  - 提供无边框 Win11 风格桌面网格（支持自定义主题/配色/字体、文件夹整理、Ctrl 拖拽格位排斥与布局持久化）、底部 Dock 快捷栏、全局搜索与保留引号参数的自定义命令启动；
  - 支持右键直接卸载已发现的 Linux 应用或清理下载目录中的 `.deb`/`.AppImage` 安装包（`focusstage uninstall-app`）；
  - 通过 `focusstage sync-apps` 同步标准 `.desktop` 条目到 Windows 开始菜单（`Programs\FocusStage\`）和桌面快捷方式，按 Linux 图标主题生成图标，支持固定到开始屏幕和任务栏；
  - 应用发现**只依赖标准 `.desktop` 与自定义命令**；软件安装继续外包给 Ubuntu 应用中心或 `apt` 等系统工具，不在启动器内自研包格式安装器；
  - 托盘右键菜单提供全局接管、按 Stage 切换显隐/冻结/接管/关闭、按已安装 Linux 浏览器动态生成的快速启动项，以及 Win11 剪贴板同步开关。
- **舞台中心（FocusStage 内置系统视图）**：
  - 作为启动器顶层与「应用桌面」并列的独立系统视图，分四个标签页集中掌控 FocusStage 自身状态：
    - **运行舞台**：实时展示所有运行中 Stage 的显隐态、输入归属 / AI 租约，支持单个一键「收入后台 / 恢复桌面 / 立即接管 / 关闭」及「全部接管」；
    - **守护与桌面**：`focusstaged` 守护进程状态与启停、全局接管热键状态、Win11 剪贴板同步开关、开机自启与桌面/开始菜单快捷方式同步；
    - **Linux 设置**：默认浏览器与默认终端切换、`xdg-open` / 桌面门户 / `mimeapps` 健康状态检测与协议关联同步；
    - **外观与快捷栏**：多套内置主题、四色自定义配色、字体方案、网格尺寸及 Dock 快捷栏管理。
- **前端解耦与分发**：
  - WebView2 界面资源内嵌于 `FocusStage.exe`（通过 `LauncherAssetServer` 挂载），与 WSL 控制面完全解耦；
  - 提供独立终端用户安装包（`packaging/FocusStage.iss` + `scripts/install-release.ps1`）与 GitHub Actions 自动发版流（`.github/workflows/release.yml`）。源码安装保留两个入口：WSL 下运行 `scripts/install.sh`（无需 PowerShell），Windows 下运行 `scripts/install.ps1`。

---

## 5. AI 控制接口（MCP）

Linux 上的 Agent 通过 **stdio** 直接启动 `/usr/local/bin/focusstage mcp`；MCP 服务再通过本地 Unix socket 连接同一用户的 `focusstaged`，不需要网络端口。Windows 上运行的 Agent 则通过 `wsl.exe -d Ubuntu -e /usr/local/bin/focusstage mcp` 启动 WSL 内的同一服务。

### 5.1 工具集

MCP 暴露 12 个下划线命名的工具（服务端同时兼容将 `.` 归一化为 `_`）：

```text
stage_list()
    → { stages: [{ stage_id, app, title, kind, pid, display, input_owner, size, rect, frame_seq, has_surface, scale, wm_class? }] }

stage_list_apps(query?)
    → { count, apps: [{ id, name, name_zh_cn? }] }

stage_launch(app_id | app_name | app, args?, kind?, cwd?, width?, height?, show_on_desktop?=false, wm_class?, wait_ready_ms?=8000, observe?=true)
    → { stage_id, pid, status: ready | timeout | readiness_unavailable, ready?, launcher_exit?, observe_message?, ... } + PNG (when ready & observe)
    # 默认后台隐藏启动（show_on_desktop=false），异步并行等待首帧就绪，不阻塞其他 Stage 的 MCP 请求

stage_acquire(stage_id, ttl_s?=60, hide?=false, agent_name?)
    → { lease, stage_id, expires_in_s }   # 同一 Stage 同时只有一个租约；默认保持当前显隐，Desktop + Agent 态展示 AI 接管覆盖层

stage_renew(lease, ttl_s?=60)
    → 续期当前租约

stage_observe(lease | stage_id, { image?=true, som?=false, a11y?=false, include_marks?=false })
    → { stage_id, frame_seq, width, height, logical_width, logical_height, scale, som, mark_count?, marks?, a11y_tree? } + PNG
    # 仅传 stage_id 时为只读观察，不获取租约也不改变窗口显隐

stage_act(lease, actions[], { based_on?, observe?=true, settle?, settle_ms?=600, som?=false })
    → { result: ok | stale | preempted | expired | error, executed_steps, frame_seq, message? } (+ follow-up observation & PNG when observe=true)

stage_clipboard_get(lease) / stage_clipboard_set(lease, text)
    → 读写该 Stage 隔离剪贴板

stage_drop_files(lease, paths[], x?, y?)
    → ok | preempted | expired | drop_failed

stage_release(lease, show_on_desktop?=true)
    → 释放租约并按需恢复桌面窗口

stage_close(stage_id)
    → 关闭并清理指定 Stage
```

`actions[]` 支持：`move`/`hover`、`click`（支持 `clicks` / `button`）、`double_click`、`drag`（`from`/`to` 或相对位移 `by`）、`scroll`（`direction` 或 `dx`/`dy`）、`key`（单键、组合键如 `Ctrl+A` 或按键数组）、`type_text`/`type`（可选点击目标、`mode: "cover"`、`send: true`）、`wait`/`sleep`。MCP 默认返回原图并支持逻辑坐标操作；`stage_observe` 和 `stage_act` 可通过 `som: true` 启用 Set-of-Mark 打标，同一租约下生成的标号（如 `"AE"`）可作为后续 `stage_act` 的目标。

### 5.2 语义约定

- **坐标**：一律使用 Stage 主窗口左上角为原点的逻辑像素，与桌面位置、DPI 无关；
- **租约**：租约的 TTL 是空闲时长。通过租约校验并被接受的观察、动作、剪贴板和文件投递请求会重置空闲时间，`stage_renew` 可以修改 TTL。到期后自动归还控制权并松开按键和鼠标按钮。近期结束的租约可以重复释放。到期后首次释放仍可恢复窗口，但以同一 Stage 上后来发生的接管、重新申请、冻结或显隐操作为准。
- **人永远优先**：用户接管后租约立即作废，之后的 `act` 返回 `preempted`，正在执行的动作序列中止；
- **过期检测**：`act` 可携带 `based_on`，如果 Stage 画面在该帧之后已发生变化，返回 `stale`，避免 AI 对着旧画面点错；
- **原子性**：一次 `act` 中的动作按顺序执行，任一步失败即中止，并返回已执行的步数。

---

## 6. 实施与收敛路线

### M0：多焦点隔离验证（已完成）

- 在 WSL2 中用 Rust + Smithay 实现无头合成器（Pixman 渲染），验证 Chromium、终端、X11 应用同时独立接收输入、互不串扰且按需合成截图。

### M1：AI 后台并发可用（已完成）

- 打通 Control API、MCP 服务（`wsl.exe` stdio）、AI 租约、AT-SPI 与纯视觉 SoM 打标，实现 AI 在后台并发操作多个 Stage 且不抢占宿主机焦点。

### M2：桌面三态闭环（已完成）

- 用早期的 WSLg 实现与托盘热键验证桌面、后台和接管闭环。测试中的标题栏和任务栏限制推动了 Win32 原生宿主的开发。

### M3：日常可用基础能力（已完成）

- 打通剪贴板双向同步与 AI 态隔离、中文输入（`text-input-v3` / Fcitx5）、高 DPI 缩放、文件拖放，以及图形化启动器与 Windows 快捷方式同步。

### M4：原生 Win32 宿主与架构收敛（核心闭环已实现）

- **已完成**：
  - `NativeSink` + `FocusStage.exe --window-host` 原生无标题栏宿主与显示后端单轨化（移除 WSLg 嵌套后端），支持 Direct2D 立即呈现、大帧可选 LZ4 压缩、同 Stage 多窗口、任务栏分组、动态 DPI（含 XWayland 分数缩放对齐）、新窗口居中、Wayland 标题栏拖动/缩放输入、切窗防漏 `Alt` 及双端文本剪贴板同步；
  - WSL2 图形加速默认路径（Chromium/Electron 默认内置 XWayland + Mesa D3D12）与通用进程归属/生命周期（`FOCUSSTAGE_LAUNCH_TOKEN` 匹配、壳进程脱离保留、`stderr` 透出、Snap/AppImage 兼容、单实例应用复用）；
  - 统一桌面会话与认证：`SessionEnv` 同步 WSL 桌面环境，内置原生 Polkit D-Bus 代理联动 Win32 提权弹窗（替代 `policykit-1-gnome`），按 Linux 图标主题解析应用图标并支持 `focusstage uninstall-app`；
  - 完整 MCP 应用工作流（`stage_list_apps`、`stage_launch` 默认后台启动与就绪首帧截图、`stage_observe` 无租约只读观察、`stage_act` 稳定等待控制、`stage_drop_files` 文件拖放、默认原图 + 按需 `som: true`）以及 `Desktop + Agent` 窗口的 AI 接管覆盖层（内发光边框、状态胶囊一键接管、虚拟指针动画）；
  - WebView2 启动器重构与独立「舞台中心」视图（运行舞台 / 守护与桌面 / Linux 设置 / 外观与快捷栏）、桌面布局持久化、托盘动态浏览器快捷启动，以及独立终端用户安装包（Inno Setup + `install-release.ps1`）、开发部署旁路 MSIX 虚拟化/支持 WSL 源码直编和 GitHub Actions 发版流。
- **按需增强项（非阻塞主闭环）**：
  1. **托管浏览器 CDP 联动**：为托管浏览器按需预留远程调试端口，进一步衔接 Playwright / Chrome DevTools MCP；
  2. **Win32 IME 直通**：当 WSL 内 Fcitx5 配置成为主要使用门槛时，推进由 `NativeHost` 直接承接 Win32 IME 上屏并转入 `zwp_text_input_v3`；
  3. **舞台中心缩略图与沙箱**：按需补充运行舞台实时缩略图/最近 AI 动作摘要，以及基于 `bubblewrap` 的可选目录隔离沙箱。

### M5：独立开机形态（远期）

- 实现 `DrmSink`（DRM/KMS + libinput），脱离 Windows 独立运行。

---

## 7. 风险与应对

| 风险 | 影响 | 应对 |
| :--- | :--- | :--- |
| 包装脚本/Snap/Electron 壳进程脱离父进程后退出 | Stage 被提前销毁或窗口无法归属，应用点开无反应 | 已通过 `FOCUSSTAGE_LAUNCH_TOKEN` 进程归属 + 脱离父进程保留 + 启动 `stderr` 透出与 `launcher_exit` 回传解决，持续关注特殊包装器兼容 |
| 自研 Win32 宿主陷入无止境细节坑 | IME、鼠标捕获、光标、缩放等边界问题拖慢迭代 | 聚焦“存在方式自然”，优先修共性机制问题；IME 后续评估 Win32 直通 `text-input-v3` |
| 启动器膨胀成自研 Linux 应用商店 | 陷入 `.deb`/Snap/AppImage 碎片兼容无底洞 | 应用安装外包给应用中心/`apt`，启动器只认标准 `.desktop` 与自定义命令，UI 封板 |
| 同款单实例应用由多个 Stage 启动 | 新 Stage 收不到窗口，或打断现有租约 | 将启动和回调统一交给现有 Human Stage，拒绝 Agent、Frozen 与歧义目标 |
| `/mnt/c` 9P 性能差 | 大仓库读写卡顿 | 文档说明，性能敏感项目放 WSL 原生文件系统 |
| AI 可读写真实用户目录 | 误删误改文件 | 后续为 Stage 增加可选沙箱（bubblewrap，目录白名单 / 只读挂载） |

---

## 8. 明确不做的事

- **不按窗口分配 `wl_seat`**：依赖客户端的多 Seat 支持，兼容性没有保证。
- **不把 Linux 软件改造成 Windows 交互体验**：追求的是像 Windows 软件一样的**存在方式与启动/管理便捷**，不魔改 Linux 应用内部手感。
- **不自研 Linux 应用商店与包格式安装器**：软件安装交给系统应用中心或包管理器，启动器只负责基于 `.desktop` 的统一呈现、整理与启动。
- **不为单个软件堆砌零碎补丁**：启动失败、后台脱离、窗口归属一律通过通用机制解决。
- **不做持续离屏合成**：只在 AI 观察、状态预览或桌面展示需要时合成。
- **Windows 侧不承载合成器业务逻辑**：启动器、内置系统软件与宿主只做呈现与入口，状态单一事实来源始终在 `focusstaged`。
- **不接管已在 Win11 原生运行的 `.exe`**：纯 Windows 软件仍由 MarkScreen 等外层方案处理。

---

## 9. 与 MarkScreen 及社区方案的关系

- **与 MarkScreen 的分工**：沿用 MarkScreen 的纯视觉 SoM 定位与"人随时可打断接管"原则，把执行载体升级为合成器级按 Stage 隔离输入；MarkScreen 继续处理只能在 Windows 上运行的原生软件，FocusStage 承载跨平台与 Linux 桌面客户端的后台并发操作。
- **与 Playwright / CDP MCP 的互补**：纯浏览器自动化用 CDP / Playwright 更轻量高效；FocusStage 的核心不可替代价值在于**非浏览器桌面客户端（微信、QQ、飞书、WPS、IDE 等）**，以及**人和 AI 共用同一个登录态与工作窗口、随时无感热切换**。
