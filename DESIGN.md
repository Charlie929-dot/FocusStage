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
- **独特价值做成启动器内的“系统软件”**：
  - 启动器本身承担类似应用桌面/抽屉的职责，帮助用户方便地发现、启动和整理应用；
  - FocusStage 区别于普通启动器的核心价值——**运行中 Stage 状态观测、前后台切换、AI 租约监控、守护进程管理与全局接管设置**——以启动器内一个专门的**内置系统软件**承载（由现有「设置」界面雏形演进），而不是塞在丑陋的右键托盘菜单里，也不让整个启动器膨胀成复杂的全能应用商店。

### 1.3 为什么是「WSL2 + 自研 Wayland 合成器」

| 路线 | 结论 | 原因 |
| :--- | :--- | :--- |
| Win32 消息 / UIA / DLL 注入 | 放弃 | Chrome/Electron/自绘客户端不理会后台 `PostMessage`，UIA 覆盖不全，注入易触发风控或崩溃 |
| Windows 虚拟机 | 放弃 | VM 内部依然是单焦点，多个窗口不能并发；镜像与内存开销重 |
| 直接用 WSLg | 不够 | WSLg 的 Weston 把所有窗口放在同一套焦点里，无法按窗口隔离输入与离屏托管；其 RAIL 强制标题栏与任务栏外观也不可控 |
| **自研 Wayland 合成器（跑在 WSL2）** | **采用** | 合成器是用户态程序，**每个输入事件送给谁完全由我们决定**；配合原生 Win32 宿主即可实现干净的窗口存在方式，核心不绑定 Windows，远期可换 DRM/KMS 后端独立开机 |

### 1.4 设计原则

1. **只自研不可替代的部分**：多焦点隔离、离屏托管与原生无标题栏 Win32 宿主（`NativeHost`）必须自己掌控；Linux 应用安装与分发外包给系统自带的应用中心 / `apt` 等标准机制，不自建包管理器。
2. **单轨收敛，避免双套并养**：显示后端收敛到单一 `NativeHost`，停止长期同时维护 WSLg 与 Native 两套显示协议，减少状态分叉与维护心智。
3. **通用机制优先，不做逐应用打补丁**：进程归属、后台脱离存活、失败日志观测都通过通用机制解决，而不是针对每个软件堆砌特判。
4. **Windows 侧只做交互与呈现壳，核心逻辑全在 WSL 内核**：启动器、内置系统软件和窗口宿主负责入口、展示、热键与原生窗口呈现；所有 Stage 状态、路由与租约逻辑集中在 `focusstaged`。

---

## 2. 核心模型

### 2.1 Stage：隔离单位是应用实例，不是单个窗口

**Stage（舞台）= 一个被托管的应用实例（进程树）**，包含它的所有顶层窗口、弹窗、子 surface。

- 应用必须通过 FocusStage 启动（`stage.launch`），不能中途接管已经在 Win11 上运行的 `.exe`。
- **每个 Stage 彼此进程隔离**：
  - Chromium / Electron：每个 Stage 使用独立的 `--user-data-dir`（或等价参数），避免多个窗口落在同一个浏览器进程里；
  - X11 应用：每个 Stage 配一个独立的 rootless `Xwayland` 实例。
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
 │  FocusStage.exe (.NET 8, UI 壳 + 原生宿主)   │                  │
 │   · 图形化启动器：应用网格 / 文件夹 / 快捷方式同步 / 应用中心入口 │
 │   · 内置系统软件（由设置面板演进）：Stage 状态监视 / AI 租约查看  │
 │     / 一键隐藏、显示、接管、关闭 / 守护进程与热键配置            │
 │   · 托盘后台常驻：全局接管热键                                  │
 │   · --window-host：原生 Win32 宿主，每顶层窗口对应一个 HWND     │
 └───────────────────────▲────────────────────┼──────────────────┘
                         │ BGRA 帧 / 键鼠、窗口状态与 DPI（stdio）
                        WSL2                  ▼
 ┌──────────────────────────────────────────────────────────────┐
 │  focusstaged  (Rust + Smithay, 单进程守护)                     │
 │                                                              │
 │   Stage Manager   通用应用启动 / 进程树归属 / Xwayland 生命周期 │
 │   Router          虚拟焦点 · 输入归属 · AI 租约               │
 │   Offscreen Core  虚拟 wl_output · frame 节拍 · 按需截图       │
 │   Clipboard       每个 Stage 一份选区，按策略与 Windows 互通    │
 │   A11y            AT-SPI 无障碍树（按 PID 归属到 Stage）        │
 │   Control API     Unix socket，供 MCP / 启动器 / CLI 使用     │
 │                                                              │
 │   DisplaySink 接口 ──┬── NativeSink  主力且唯一收敛目标（Win32）│
 │                      ├── WslgSink    M2 过渡实现（待清理移除）  │
 │                      └── DrmSink     远期：独立开机            │
 │                                                              │
 │        WAYLAND_DISPLAY=focusstage-0  /  每 Stage 一个 Xwayland │
 │              ▼                ▼                ▼              │
 │         [Chromium]      [Electron 应用]    [X11 应用]          │
 │                                                              │
 │   文件直通：/mnt/c/Users/<user>/...（与 Win11 同一份文件）       │
 └──────────────────────────────────────────────────────────────┘
```

**业务逻辑全部集中在 WSL 内的 `focusstaged` 一个进程里**。Windows 侧（`FocusStage.exe`）承担交互壳与显示宿主：提供便捷的应用启动/管理入口、内置系统信息与控制界面、全局接管热键以及原生 Win32 窗口渲染。

---

## 4. 模块设计

### 4.1 Stage Manager

- 根据应用描述（命令、参数、类型 Wayland/X11/Chromium/Electron）启动进程，注入 `WAYLAND_DISPLAY`、`DISPLAY`、`FOCUSSTAGE_STAGE_ID` 等运行环境；
- X11 Stage：先拉起专属的 rootless `Xwayland`（合成器内置 XWM），再在该 `DISPLAY` 上启动应用；
- **通用进程归属与生命周期（收敛重点）**：
  - 不假设“初始启动命令就是常驻 GUI 主进程”——Snap、Electron、IDE 包装脚本常在拉起后台 GUI 后自行退出；
  - 进程归属结合 Stage 标识环境（如 `FOCUSSTAGE_STAGE_ID`）、独立隔离边界与进程树信息，而不仅依赖初始 PID 的单层父子链；
  - 初始壳进程正常退出时不立即误杀 Stage，结合宽限期与窗口/存活进程状态统一回收；异常退出时记录 `stderr` 摘要至守护进程日志，避免静默失败。

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

**NativeSink（主力后端，唯一收敛目标）**

- `focusstaged` 通过 WSL 互操作启动 Windows 侧 `FocusStage.exe --window-host`，以 stdio 二进制协议传输 BGRA 像素帧与键鼠/窗口/DPI 事件；
- 每个 Desktop 态顶层窗口对应一个独立的无边框 Win32 `HWND`：保留 Linux 应用自身的 CSD 标题栏控件，不叠加 Windows 标题栏，支持 Win11 原生圆角、任务栏按 Stage 分组（`AppUserModelID`）及同 Stage 多窗口；
- 关闭单个宿主窗口仅向对应 Linux 窗口发送关闭请求；收入后台即隐藏宿主窗口并保留 `SavedRect`，恢复时原位呈现；
- 跨显示器移动时，向支持 `wp_fractional_scale_v1` 的 Wayland 客户端同步该窗口的 DPI 更新；X11 客户端走 XWayland 缩放路径。

**WslgSink（M2 历史过渡实现，计划从主线移除）**

- 早期用于验证桌面三态闭环，但受限于 WSLg RAIL 的强制标题栏、企鹅角标与多窗口控制能力，已不再作为演进方向；
- 当前代码暂保留 `--wslg` 作为应急回退，待 `NativeSink` 稳定性验证完成后整套清理，实现显示后端单轨维护。

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

- 引导应用的默认目录（XDG User Dirs）指向 `/mnt/c/Users/<user>/` 下的桌面、下载等目录；
- 已知限制：`/mnt/c` 走 9P 文件系统，大量小文件读写（如大仓库构建）明显变慢，对性能敏感的项目建议放在 WSL 原生文件系统中；
- 从资源管理器拖文件到宿主窗口：路径 `C:\...` 转为 `/mnt/c/...`，经 `wl_data_device` 投递拖放；
- 链接打开 / "在文件夹中显示"：按策略交给托管浏览器或 Win11 原生程序。

### 4.9 Windows 启动器与内置系统软件

`FocusStage.exe`（`tray/`）是面向用户的 Windows 交互入口，但严格保持为**展示与入口壳**，不承载合成器核心业务逻辑：

- **图形化应用启动与管理（原生存在方式）**：
  - 提供无边框桌面风格的应用网格、文件夹整理、自定义命令启动与搜索；
  - 通过 `focusstage sync-apps` 同步标准 `.desktop` 条目到 Windows 开始菜单（`Programs\FocusStage\`）和桌面快捷方式，支持固定到开始屏幕和任务栏；
  - 应用发现**只依赖标准 `.desktop` 与自定义命令**；软件安装继续外包给 Ubuntu 应用中心（Dock 快捷入口）或 `apt` 等系统工具，不在启动器内自研包格式安装器。
- **内置系统软件：FocusStage 控制与状态中心（独特价值承载面）**：
  - 作为启动器内建的一个系统级软件（由当前「设置」覆盖层中的守护进程状态与「运行中的 Stages」列表雏形演进为独立清晰的内置应用视图），专门用于查看和掌控 FocusStage 自身信息：
    - 守护进程健康状态、启动/重启与全局接管热键配置；
    - 所有运行中 Stage 的实时信息（前后台状态、人类控制/AI 租约持有者、窗口列表，后续可扩展实时缩略图与最近 AI 动作摘要）；
    - 对单个 Stage 一键「收入后台 / 恢复桌面 / 立即接管 / 关闭」，以及一键全部接管。
- **前端演进边界**：
  - 现有 WinForms 启动器完成“设置 → 内置系统信息/控制软件”的结构整理后进入功能冻结，避免在 GDI+ 自绘上无止境堆叠动效；
  - 因 UI 与 WSL 控制面完全解耦，未来若需更现代的桌面 UI，可直接替换前端展示壳（例如单窗口 WebView2）而不改动合成器内核与 `NativeHost`。

---

## 5. AI 控制接口（MCP）

Windows 上的 Agent 以 **stdio** 方式连接：MCP 配置中把命令设为 `wsl.exe -d Ubuntu -e focusstage mcp` 即可，不需要网络端口，也不涉及跨 VM 的 socket。

### 5.1 工具集

```text
stage.list()
    → [{ stage_id, app, title, display, input_owner, size }]

stage.launch(app, args?)
    → stage_id

stage.acquire(stage_id, ttl_s, hide?=true)
    → lease                         # 同一 Stage 同时只有一个租约

stage.observe(lease, { image?: bool, a11y?: bool })
    → { frame_seq, png?, a11y_tree? }

stage.act(lease, actions[], based_on?: frame_seq)
    → ok | stale | preempted | expired

stage.clipboard_get(lease) / stage.clipboard_set(lease, text)

stage.release(lease, show_on_desktop?=true)
```

`actions[]` 支持：`move`、`click`、`double_click`、`drag`、`scroll`、`key`（含组合键）、`type_text`、`wait(ms)`；MCP 层同时结合纯视觉 Set-of-Mark（SoM）打标支持双字母短码与坐标操作。

### 5.2 语义约定

- **坐标**：一律使用 Stage 主窗口左上角为原点的逻辑像素，与桌面位置、DPI 无关；
- **租约**：`acquire` 失败时返回当前持有者；租约到期未续自动释放；
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

- 通过 `WslgSink` 与托盘/热键验证「桌面正常使用 ↔ 一键收入后台 ↔ AI 操作 ↔ 热键原位恢复」完整闭环；实测发现 WSLg 强制标题栏与外观无法满足原生存在方式要求，触发向原生宿主切换。

### M3：日常可用基础能力（已完成）

- 打通剪贴板双向同步与 AI 态隔离、中文输入（`text-input-v3` / Fcitx5）、高 DPI 缩放、文件拖放，以及图形化启动器与 Windows 快捷方式同步。

### M4：原生 Win32 宿主与架构收敛（当前阶段）

- **已完成**：`NativeSink` + `FocusStage.exe --window-host` 原生无标题栏宿主、同 Stage 多窗口与任务栏分组、启动器基础形态与设置/运行中 Stages 雏形。
- **当前收敛与加固重点**：
  1. **显示后端单轨化**：修稳 `NativeHost` 的派发、输入、IME 拦截、窗口尺寸同步与离屏激活状态后，彻底移除主线中的 `WslgSink`，结束双套显示协议并养；
  2. **通用启动与进程生命周期**：解决包装脚本/Snap/Electron 壳进程脱离后台导致的误回收与窗口失联，并向日志透出启动失败错误，替代按单软件打补丁；
  3. **内置系统软件成形**：将启动器中现有的「设置 + 运行中的 Stages」整理为清晰的内置系统软件（用于集中查看守护进程、Stage 状态、AI 租约与执行切换/接管），启动器其余外围功能封板；
  4. **浏览器与 CDP 互补（按需）**：为托管浏览器预留远程调试端口能力，让纯网页任务可直接衔接 Playwright / Chrome DevTools MCP，FocusStage 聚焦桌面客户端与人机共享会话。

### M5：独立开机形态（远期）

- 实现 `DrmSink`（DRM/KMS + libinput），脱离 Windows 独立运行。

---

## 7. 风险与应对

| 风险 | 影响 | 应对 |
| :--- | :--- | :--- |
| 双显示后端（`NativeSink` + `WslgSink`）长期并存 | 窗口激活、焦点、DPI 语义互相掣肘，维护成本翻倍 | 明确单轨收敛到 `NativeSink`，稳定后彻底移除 `WslgSink` |
| 包装脚本/Snap/Electron 壳进程脱离父进程后退出 | Stage 被提前销毁或窗口无法归属，应用点开无反应 | 通过通用进程归属（环境变量/隔离边界）+ 宽限期/窗口存活回收 + 启动 stderr 日志彻底解决 |
| 自研 Win32 宿主陷入无止境细节坑 | IME、鼠标捕获、光标、缩放等边界问题拖慢迭代 | 聚焦“存在方式自然”，优先修共性机制问题；IME 后续评估 Win32 直通 `text-input-v3` |
| 启动器膨胀成自研 Linux 应用商店 | 陷入 `.deb`/Snap/AppImage 碎片兼容无底洞 | 应用安装外包给应用中心/`apt`，启动器只认标准 `.desktop` 与自定义命令，UI 封板 |
| 一个 Stage 被多个进程共享（如浏览器复用已有进程） | 虚拟焦点产生矛盾 | 强制独立 `--user-data-dir` / 独立 Xwayland |
| `/mnt/c` 9P 性能差 | 大仓库读写卡顿 | 文档说明，性能敏感项目放 WSL 原生文件系统 |
| AI 可读写真实用户目录 | 误删误改文件 | 后续为 Stage 增加可选沙箱（bubblewrap，目录白名单 / 只读挂载） |

---

## 8. 明确不做的事

- **不按窗口分配 `wl_seat`**：依赖客户端的多 Seat 支持，兼容性没有保证。
- **不再长期并养两套显示后端**：明确以 `NativeHost` 为唯一演进主线，不再同时维护 `WslgSink`。
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
