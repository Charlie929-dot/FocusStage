# FocusStage 架构设计文档

## 1. 设计目标与问题根源

### 1.1 我们真正想解决什么问题
在桌面 AI Agent 协作场景下，理想的人机共处体验是：
1. **平时无感使用**：软件以独立原生窗口形态摆在你的日常桌面上，大小、位置、任务栏排队与普通程序一致，文件直接读写真实工作目录。
2. **随时收入后台独立操作**：当你把某个窗口「收入后台」交给 AI 时，它在后台保持全速渲染，AI 拥有**该窗口专属的虚拟鼠标、键盘、焦点与剪贴板**；你的物理鼠标、当前正在打字的窗口和宿主剪贴板**零干扰**。
3. **多窗口真并发**：可以同时把窗口 A 交给 AI 1、窗口 B 交给 AI 2，同时自己在主桌面操作窗口 C，三者互不抢焦点。
4. **随时释放与恢复**：任务完成（或用户主动点接管）后，窗口按收入前的桌面坐标、尺寸原地恢复，画面与登录态 100% 连续，不需要重连、同步或二次回放动作。

### 1.2 为什么选择「Win11 宿主壳 + WSL2 自研 Wayland 合成器」
- **放弃纯 Win32 消息/UIA 劫持**：现代浏览器（Chrome/Electron）与自绘客户端不理会后台 `PostMessage`，UIA 覆盖面不足，DLL 注入 Hook 极易触发反注入风控或崩溃。
- **放弃重型 Windows 虚拟机底座**：同一个 Windows 会话内部依然是全局单焦点（人机不能并发），且跨界文件、内存占用和桌面套娃体验很重。
- **选择 WSL2 Wayland Multi-Seat 合成器**：
  - Linux 显示架构（Wayland）天然把“窗口合成”与“内核驱动”解耦；
  - 合成器是用户态程序，可由我们完全掌控：**一套内核驱动之上，同时挂载一个绑定真实交互的物理/宿主 Seat，以及 N 个分别绑定单窗口的虚拟 AI Seat**；
  - 跑在 Win11 的 WSL2 内时，由 Win11 外层接管电源、休眠、Wi-Fi、双显卡调度与偶尔需要的 Windows 原生软件；核心代码本身不绑定 Windows，未来可直接切换到 Linux DRM/KMS 后端独立开机运行。

> **为什么不直接用 WSLg？**  
> WSLg 本质上只是在 WSL2 里跑了一个面向整个 Linux 桌面的单焦点 Weston/RDP 合成器，它本身依然把所有 WSL 窗口塞进同一套鼠标/焦点中，无法实现“按窗口分配独立虚拟 Seat + 离屏渲染托管 + 一键从桌面收进/放回”。因此需要自研一个**轻量专用的 Wayland 合成器**取代 WSLg 的窗口管理职责（仅在 WSL2 内监听独立 `WAYLAND_DISPLAY` Socket）。

---

## 2. 核心生命周期：单窗三态模型

FocusStage 对每一个纳入管理的顶层应用窗口分配全局唯一 `window_id`，它在任意时刻处于以下三种状态之一：

```text
                 [1] 用户点击「收入窗口 / 交给 AI」
  ┌───────────────────┐ ─────────────────────────────► ┌──────────────────────────┐
  │  HumanAttached    │                                │        AiStaged          │
  │ （Win11 桌面使用）│ ◄───────────────────────────── │（后台离屏 + 独立 Seat 操作）│
  └───────────────────┘   [2] AI 完成 / 用户点「接管」 └──────────────────────────┘
          │                                                       │
          └──────────────► (共享同一个活着的 Wayland Surface) ◄───┘
```

### 状态 A：`HumanAttached`（真机桌面使用态）
- **Win11 侧**：`FocusStage.Host` 为该 `window_id` 创建一个原生无边框（带极简顶栏条）的宿主窗口，保留收入前的坐标 `(x, y, width, height)`，占独立任务栏项。
- **合成器侧**：将该窗口绑定到 **Human Seat（`seat_human`）**；Win11 宿主壳捕获的鼠标移动、点击、滚轮、键盘和拖拽事件经由轻量 IPC 实时注入 `seat_human`，合成器将窗口画面帧回传给宿主壳渲染。

### 状态 B：`AiStaged`（收入后台独立操作态）
- **Win11 侧**：
  - 原先占屏幕的大窗口立即隐藏（仅隐藏壳窗体，保留原坐标快照 `SavedRect`）；
  - 可选地收入统一的「AI 后台工作台 / 只读围观画中画小窗」中供用户随时余光观察进度（默认禁止宿主鼠键误触透传）。
- **合成器侧**：
  - **关键机制（离屏持续渲染）**：合成器**绝对不向目标应用发送“最小化 / 失焦 / 不可见”通知**，而是将该窗口的 Surface 挂入离屏舞台（Offscreen Stage），维持正常 `wl_surface.frame` 回调驱动（如 30~60fps 按需节拍），欺骗应用认为自己仍处于完整激活显示状态；
  - 为该窗口解绑 `seat_human`，绑定专属的 **虚拟 AI Seat（`seat_ai_<window_id>`）**，并向该窗口发送激活态（`activated`），使其内部输入框、悬停态、弹出菜单正常工作；
  - 所有 AI 视觉截屏与输入动作仅作用于该 `window_id` 及其对应的 `seat_ai_<window_id>`。

### 状态 C：释放并恢复（`AiStaged` → `HumanAttached`）
- **触发方式**：
  1. AI 任务正常结束并调用 `release_window(window_id)`；
  2. 用户在围观小窗随时点击 **「接管 / 释放恢复」**（支持触发硬中断信号，立刻冻结该窗口的 AI Seat 写入权，防止人机抢最后一帧点击）。
- **切换动作**：
  - 合成器立即断开或静默 `seat_ai_<window_id>`，重新激活 `seat_human` 并把最新画面帧推给 Win11 宿主壳；
  - `FocusStage.Host` 按照 `SavedRect` 在桌面原位置、原大小瞬间弹出窗口，用户从 AI 停下的那一刻无缝接手。

---

## 3. 总体架构与四大模块

```text
┌────────────────────────────────────────────────────────────────────────────┐
│ Windows 11 宿主机                                                           │
│                                                                            │
│  ┌─────────────────────────┐    ┌───────────────────────────────────────┐  │
│  │ FocusStage.Host (.NET 8)│    │ AI Agent (Claude / Cursor / 本地脚本) │  │
│  │ - 一窗一壳原生窗口管理  │    └───────────────────┬───────────────────┘  │
│  │ - 只读围观小窗/接管按钮 │                        │ MCP (stdio / SSE)    │
│  │ - 宿主鼠键/拖放/IME转发 │                        ▼                      │
│  └────────────┬────────────┘    ┌───────────────────────────────────────┐  │
│               │                 │ FocusStage MCP Server (窗口级控制面)   │  │
│               │                 └───────────────────┬───────────────────┘  │
└───────────────┼─────────────────────────────────────┼──────────────────────┘
                │ 本地高速通信 (Unix Socket / Loopback TCP / 共享内存映射)
┌───────────────┼─────────────────────────────────────┼──────────────────────┐
│ WSL2 (Ubuntu) │                                     │                      │
│               ▼                                     ▼                      │
│  ┌──────────────────────────────────────────────────────────────────────┐  │
│  │ FocusStage Compositor (Rust / Smithay Wayland + XWayland)            │  │
│  │                                                                      │  │
│  │  • Window Registry (window_id 映射 TopLevel / Popup / SavedRect)     │  │
│  │  • Seat Manager    (seat_human + 每窗口独立 seat_ai_<id>)             │  │
│  │  • Offscreen Stage (无头渲染器，持续驱动 frame callback & 抓帧)       │  │
│  │  • Clipboard Vault (AI 操作期间按 Seat 隔离剪贴板，不污染宿主)        │  │
│  └──────────────────────────┬───────────────────────────┬───────────────┘  │
│                             │ WAYLAND_DISPLAY=focus-0   │                  │
│                             ▼                           ▼                  │
│              [ Chromium / Web 应用 ]     [ 飞书 / 微信 / VS Code / 工具 ]   │
│                                                                            │
│  文件系统直通：/mnt/c/Users/HP/Desktop, Downloads, WorkBuddy ... (同一份文件) │
└────────────────────────────────────────────────────────────────────────────┘
```

### 模块一：WSL2 Wayland Multi-Seat 合成器（`compositor/`）
- **推荐技术栈**：Rust + [`Smithay`](https://github.com/Smithay/smithay)（相比 C `wlroots`，Rust 类型安全且在动态创建/销毁 `Seat`、无头离屏渲染 `Renderer` 和多线程异步控制面通信上更易维护）。
- **核心职责**：
  1. **协议监听**：在 WSL2 内创建独立 Wayland Socket（例如 `WAYLAND_DISPLAY=focusstage-0`）及配套 `XWayland`（兼容 Electron / 旧版 X11 软件）。
  2. **按窗口多 Seat 隔离**：
     - 维护 `seat_human`（供所有处于 `HumanAttached` 状态的窗口共享）以及按窗口按需分配的 `seat_ai_<id>`；
     - 注入点击/滚轮/键盘时，直接向目标窗口对应的 `seat_ai_<id>` 发送 `pointer_motion`、`button`、`key`、`axis` 事件，并确保 `wl_keyboard.enter` / `wl_pointer.enter` 只投递给该 `window_id` 及其子弹窗（Popup/Menu）。
  3. **离屏帧合成（包含主窗 + 悬浮弹窗）**：
     - 浏览器右键菜单、下拉列表在 Wayland 中往往是独立 `xdg_popup` 或子 Surface。合成器需要把主窗口与它的所有关联 `xdg_popup` 合成为一张以主窗口为原点的完整画布帧，这样 AI 截图和宿主壳显示都不会丢失右键菜单和下拉框。
  4. **独立剪贴板与选区（Data Device Isolation）**：
     - 在 Wayland 中剪贴板归属于 `wl_seat`（`wl_data_device`）。每个 `seat_ai_<id>` 天然拥有自己的剪贴板状态，AI 在窗口 A 里 Ctrl+C/V 完全不会覆盖宿主 Win11 剪贴板，也不会串到窗口 B。

### 模块二：Win11 原生宿主壳（`host/`）
- **推荐技术栈**：`.NET 8` + `WPF / Win32 P/Invoke`（本机已安装 `.NET 8.0.428`）。
- **核心职责**：
  1. **一窗一壳**：收到合成器上报的新顶层窗口事件后，在 Win11 创建独立窗口壳（可配置窄控制条：显示标题、图标、**「收入后台交给 AI」**按钮）。
  2. **高效画面呈现**：使用 `WriteableBitmap` / D3DImage 或 SwapChain 渲染合成器推送的窗口增量帧（MVP 阶段可先走本地共享内存 / Loopback 原始 BGRA 帧或快速 PNG/LZ4 块，仅对有变化且正在显示/围观的窗口推帧）。
  3. **AI 围观面板**：所有处于 `AiStaged` 状态的窗口汇总在宿主壳的「后台队列抽屉 / 只读画中画窗」中，显示实时缩略图与 AI 动作状态，并提供一键 **「释放并恢复到桌面」** 按钮。
  4. **窗口定位与尺寸同步**：当用户拖拽缩放 Win11 壳窗口时，发回 `configure(width, height)` 给合成器通知目标程序重排布局（响应式重绘，而非简单图像拉伸）。

### 模块三：跨界体验桥（`bridge/`）
补齐“感觉就是真机窗口”的最后 10% 细节：
1. **文件共享与拖放**：
   - 引导 WSL 侧应用默认工作目录指向 `/mnt/c/Users/HP/Desktop`、`/mnt/c/Users/HP/Downloads` 等真机路径（或配置 XDG User Dirs 直接映射到 Win11 用户目录）。
   - 当用户把文件从 Win11 资源管理器拖到壳窗口上时，宿主壳将 Windows 路径 `C:\...` 转换为 WSL 路径 `/mnt/c/...`，通过 Wayland `wl_data_device` 模拟拖放事件投递给目标窗口。
2. **输入法（IME）策略**：
   - **Phase 1（稳妥方案）**：在 WSL2 内启用 `Fcitx5`（挂载 `zwp_text_input_v3` / `input_method_v2`），由合成器把输入法候选窗口作为目标窗口的子图层一起合成输出——候选框永远精准跟随光标，且不与 Win11 输入法冲突。
   - **Phase 3（可选体验增强）**：支持从 Win11 宿主壳捕获中文 IME 组合结果（`WM_IME_COMPOSITION`）直发 `text_input_v3` preedit/commit，实现外层单一输入法体验。
3. **链接打开与文件关联（可选）**：
   - WSL 侧应用点击外部链接或“在文件夹中显示”时，按策略路由给 WSL 托管浏览器或回调 Win11 原生程序。

### 模块四：AI 窗口级控制面与 MCP 服务（`mcp/`）
面向外部 AI Agent 提供简洁、确定的工具集（以 `window_id` 为一等公民，彻底摆脱全屏坐标和焦点抢占）：
- `stage_list_windows()` — 列出当前所有窗口及状态（`HumanAttached` / `AiStaged`）、标题、尺寸与可用状态。
- `stage_acquire(window_id)` — 请求将指定窗口收入后台（或仅操作已由用户主动收入 `AiStaged` 的窗口；支持权限策略开关）。
- `stage_observe(window_id)` — 获取该窗口最新一帧画面（可选叠加轻量坐标网格 / MarkScreen 式标注），即使它不在桌面显示也能满帧抓图。
- `stage_act(window_id, actions)` — 向 `seat_ai_<window_id>` 注入原子鼠键动作序列（点击、移动、拖拽、滚轮、键入文本、快捷键）；若期间窗口已被用户手动「接管释放」，立即返回中断状态并中止后续动作。
- `stage_release(window_id)` — 操作完成，将窗口释放并恢复至用户 Win11 桌面原位。

---

## 4. 与现有 MarkScreen 的关系

- **设计理念继承**：沿用 [MarkScreen](C:/Users/HP/WorkBuddy/MarkScreen) 的视觉定位与“人随时可打断接管”原则，但把执行载体从“全局物理桌面的单钩子模式”升级为“**单窗口独立 Seat 的合成器隔离模式**”。
- **互不阻塞**：MarkScreen 依然可留在 Win11 外层，处理极少数无法在 Linux 运行的原生 Windows 软件；FocusStage 作为新一代通用窗口容器，承载浏览器与跨平台客户端的后台并发操作。

---

## 5. 分阶段实施路线（严格按最小可验证增量推进）

### Phase 0：WSL2 合成器工具链验证（1~2 天）
- 目标：在 WSL2 `Ubuntu 26.04.1 LTS` 中安装 Rust 工具链与 `Smithay` 编译依赖（`libwayland-dev`, `libxkbcommon-dev`, `libgbm-dev`, `libinput-dev` 等）。
- 验收：编写一个极简无头 Wayland 合成器进程，能以 `WAYLAND_DISPLAY=focus-test` 启动一个标准 Linux GUI 程序（如 `foot` 终端或 `chromium`），并在后台完成离屏渲染抓出首张 PNG 截图。

### Phase 1：单窗口「显示 ↔ 收入 ↔ 独立操作 ↔ 释放」最小闭环
- 目标：打通单窗口完整三态。
- 范围：
  - WSL2 合成器支持 1 个窗口、`seat_human` 与 `seat_ai` 切换、离屏持续渲染及帧回传（Socket）；
  - Win11 `.NET 8` 极简宿主壳显示该窗口并转发鼠键；
  - 宿主壳提供「收入后台并运行测试脚本（AI Seat 自动点击/输入）」按钮和「释放恢复」按钮。
- 验收：用户在壳里打字后点「收入后台」，壳窗消失变为角落小预览窗；后台脚本通过 `seat_ai` 在窗口内自动点击输入，期间用户在 Win11 记事本正常打字**零丢焦点、零鼠标抖动**；点「释放恢复」，窗口在 Win11 原位弹出且保留全部新内容。

### Phase 2：多窗口并发隔离与 MCP 接入
- 目标：支持同时托管 2 个以上窗口并发执行，并暴露标准 MCP 工具供 AI 直接调度。
- 范围：
  - 动态窗口注册与弹窗（`xdg_popup`）合成；
  - 多 `seat_ai_<id>` 与独立剪贴板验证；
  - 实现 `mcp/` 网关，验证 AI 同时独立操作后台浏览器窗口 A 与编辑器窗口 B。

### Phase 3：跨界日常手感打磨
- 目标：让日常使用接近 Win11 原生窗口体验。
- 范围：XWayland 兼容（覆盖 Electron/Chrome 稳定运行）、Win11 → WSL 文件拖放转换、中文输入法（`Fcitx5` 或宿主 IME 桥接）、DPI 高分屏缩放与动态 resize 同步。

### Phase 4：远期独立化（可选）
- 目标：在物理 Linux 主机/专用设备直接启动。
- 范围：为 `compositor/` 增加 DRM/KMS + `libinput` 后端，不再依赖 Win11 宿主壳，直接接管物理显示器与物理键鼠，演进为独立开机的 AI-Native 操作系统桌面。

---

## 6. 已确认的关键技术风险与应对策略

| 风险点 | 原因 | 应对策略 |
| :--- | :--- | :--- |
| **弹出菜单 / 下拉框丢失或坐标错位** | Chrome/GTK 的右键菜单是独立 `xdg_popup`，默认相对父窗口偏移 | 合成器按主窗口 `Surface` 树汇总边界，将激活中的子 `popup` 一并合成为单张窗口画布并同步边界扩展 |
| **应用在离屏后降帧或暂停动画** | 部分应用检测不到 `wl_surface.frame` 回调或收到失焦后会节能休眠 | 合成器在 `AiStaged` 态持续按固定节拍（如 30Hz）触发 `frame_callback`，并始终对托管窗保持 `activated` 状态 |
| **Win11 ↔ WSL2 传帧延迟或带宽过高** | 无脑全屏 60fps 传未压缩像素开销大 | 按窗口脏区（Damage Region）增量推帧；仅对处于 `HumanAttached` 或正在被用户围观/AI 观察的窗口保持活跃推帧 |
| **Electron / Chrome 走 X11 还是 Wayland** | 部分旧版 Linux 客户端默认走 X11，不设参数不连 Wayland | 默认开启 `--ozone-platform=wayland` 启动现代 Chromium/Electron，并保留 `XWayland` 兜底兼容纯 X11 程序 |
