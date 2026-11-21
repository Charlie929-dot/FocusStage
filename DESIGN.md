# FocusStage 架构设计文档

> 本文档描述 FocusStage 的目标、核心模型、模块划分、AI 控制接口与分阶段路线。
> 标注 **【待验证】** 的结论尚未经过实测，由对应里程碑负责证实或推翻。

---

## 1. 目标与问题根源

### 1.1 要解决的问题

传统桌面系统只有**一个全局鼠标、一个前台焦点、一个剪贴板**。AI Agent 想操作某个窗口，就必须抢走用户的鼠标和焦点——人和 AI 只能轮流用电脑。

FocusStage 要实现的体验：

1. **平时无感使用**：软件以普通窗口的形态出现在 Win11 桌面上，文件直接读写真实工作目录。
2. **随时交给 AI**：一键把某个窗口「收入后台」，它在后台继续正常运行，AI 独立操作它；用户的鼠标、正在打字的窗口和剪贴板**不受任何影响**。
3. **多窗口真并发**：AI 1 操作窗口 A、AI 2 操作窗口 B、用户自己操作窗口 C，三者互不抢焦点。
4. **随时收回**：AI 做完或用户按下接管热键，窗口回到原来的位置和大小，画面与登录态完全连续。

### 1.2 为什么是「WSL2 + 自研 Wayland 合成器」

| 路线 | 结论 | 原因 |
| :--- | :--- | :--- |
| Win32 消息 / UIA / DLL 注入 | 放弃 | Chrome/Electron/自绘客户端不理会后台 `PostMessage`，UIA 覆盖不全，注入易触发风控或崩溃 |
| Windows 虚拟机 | 放弃 | VM 内部依然是单焦点，多个窗口不能并发；镜像与内存开销重 |
| 直接用 WSLg | 不够 | WSLg 的 Weston 把所有窗口放在同一套焦点里，无法按窗口隔离输入，也无法把窗口离屏托管 |
| **自研 Wayland 合成器（跑在 WSL2）** | **采用** | 合成器是用户态程序，**每个输入事件送给谁完全由我们决定**；核心不绑定 Windows，远期可换 DRM/KMS 后端独立开机 |

### 1.3 设计原则

1. **只自研不可替代的部分**：多焦点隔离与离屏托管必须自己写；显示到 Windows、窗口管理等能借现成方案的先借。
2. **风险最高的假设最先验证**：每个里程碑都有明确的「不过就停」判据。
3. **可替换的边界**：显示输出、输入来源都抽象成接口，第一版实现可以被整体替换而不动核心。
4. **AI 价值先于桌面体验**：先让 AI 能在后台并发操作，再打磨人在桌面上的手感。

---

## 2. 核心模型

### 2.1 Stage：隔离单位是进程，不是窗口

**Stage（舞台）= 一个被托管的应用实例 = 一个独立的客户端进程**，包含它的所有顶层窗口、弹窗、子 surface。

- 应用必须通过 FocusStage 启动（`stage.launch`），不能中途接管已经在 Win11 上运行的 `.exe`。
- **一个 Stage 只对应一个进程**：
  - Chromium / Electron：每个 Stage 使用独立的 `--user-data-dir`（或等价参数），避免多个窗口落在同一个浏览器进程里；
  - X11 应用：每个 Stage 配一个独立的 rootless `Xwayland` 实例。
- 同一应用内部的对话框、菜单、弹窗都属于同一个 Stage，进程内部的焦点切换由应用自己处理。

> 为什么不按窗口隔离？同一进程的多个窗口共享一个 Wayland 连接，合成器无法让它们"同时都是前台"而不让客户端察觉矛盾。以进程为单位，矛盾就不存在了。

### 2.2 虚拟焦点：所有 Stage 永远处于前台

**不使用多 `wl_seat`**。原因：多 Seat 依赖客户端的支持，而 Chromium/Electron 对多 Seat 的支持很有限【待验证】，XWayland 则完全不支持多 Seat。

替代方案：

- 合成器只暴露**一个 `wl_seat`**；
- 对**每一个** Stage，合成器都发送 `wl_keyboard.enter`、`wl_pointer.enter` 和 `xdg_toplevel` 的 `activated` 状态，并且**永不发送 leave / 失活**；
- 每个 Stage 都以为自己是唯一的前台窗口。由于客户端之间互相看不到对方的 surface，这种"多个前台"不会被察觉；
- 每个输入事件最终送给哪个 Stage，由合成器内部的 **Router** 根据路由表决定。

修饰键状态（Ctrl/Shift 等）、键盘重复、指针位置都**按 Stage 分别维护**，AI 在 Stage A 按住 Ctrl 不会影响用户在 Stage B 里打字。

【待验证 · M0】Chromium、Electron、GTK、Qt、X11 应用在"永远前台、从不失焦"的状态下能否正常接收键盘输入、弹出菜单、显示光标。

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

**状态切换只修改 Router 的路由表和显示开关，应用本身完全不知道发生了什么**——它始终是前台、始终在渲染、始终持有焦点。因此画面、登录态、未保存的输入天然连续，不需要任何同步或重放。

切换规则：

- **人永远优先**：任何状态下，用户按接管热键（或点击接管按钮）都会立刻把输入归属改为 Human，并作废当前 AI 租约。
- 收入后台时记录窗口在桌面上的位置与大小（`SavedRect`），恢复时按 `SavedRect` 放回原处。
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
 │  FocusStage Tray (.NET 8, 极薄)             │                  │
 │   · 全局接管热键  · Stage 列表 / 状态       │                  │
 │   · 通过 wsl.exe 调用控制命令               │                  │
 └────────────────────────────────────────────┼──────────────────┘
                                              │
                        WSL2                  ▼
 ┌──────────────────────────────────────────────────────────────┐
 │  focusstaged  (Rust + Smithay, 单进程守护)                     │
 │                                                              │
 │   Stage Manager   启动应用 / 管理进程与 Xwayland 实例          │
 │   Router          虚拟焦点 · 输入归属 · AI 租约               │
 │   Offscreen Core  虚拟 wl_output · frame 节拍 · 按需截图       │
 │   Clipboard       每个 Stage 一份选区，按策略与 Windows 互通    │
 │   A11y            AT-SPI 无障碍树（按 PID 归属到 Stage）        │
 │   Control API     Unix socket，供 MCP / Tray / 调试工具使用    │
 │                                                              │
 │   DisplaySink 接口 ──┬── WslgSink   第一版：嵌套进 WSLg        │
 │                      ├── HostSink   备选：原生壳 + hvsocket    │
 │                      └── DrmSink    远期：独立开机             │
 │                                                              │
 │        WAYLAND_DISPLAY=focusstage-0  /  每 Stage 一个 Xwayland │
 │              ▼                ▼                ▼              │
 │         [Chromium]      [Electron 应用]    [X11 应用]          │
 │                                                              │
 │   文件直通：/mnt/c/Users/<user>/...（与 Win11 同一份文件）       │
 └──────────────────────────────────────────────────────────────┘
```

**业务逻辑全部集中在 WSL 内的 `focusstaged` 一个进程里**。Windows 侧只保留必须在 Windows 上做的事：全局热键（物理键盘在 Windows 那边）和一个状态入口。

---

## 4. 模块设计

### 4.1 Stage Manager

- 根据应用描述（命令、参数、类型 Wayland/X11）启动进程，设置 `WAYLAND_DISPLAY`、`DISPLAY`、Chromium 的 `--ozone-platform=wayland` 等环境；
- X11 Stage：先拉起专属的 rootless `Xwayland`（合成器内置 XWM），再在该 `DISPLAY` 上启动应用；
- 监控进程生命周期：进程退出则销毁 Stage，并通知 Tray 与持有租约的 Agent；
- 通过客户端凭据（PID）把 Wayland 连接、Xwayland 实例、AT-SPI 对象归属到对应 Stage。

### 4.2 Router（虚拟焦点与输入归属）

- 维护 `stage_id → { input_owner, lease, per-stage 键盘/指针状态 }`；
- 输入来源有三类：WslgSink 转来的**人工输入**、MCP 转来的 **AI 输入**、Tray 发来的**接管指令**；
- 每个事件按"目标 Stage 当前的输入归属"校验后才投递，不符合的直接丢弃；
- 接管指令是硬中断：立即切换归属、作废租约、丢弃该 Stage 队列中尚未执行的 AI 动作；
- Stage 内部若有多个顶层窗口，键盘焦点给最近激活的顶层窗口或模态对话框。

### 4.3 Offscreen Core（离屏托管与截图）

- 合成器创建一个**虚拟 `wl_output`**（固定分辨率与缩放），所有 Stage 的 surface 都 `enter` 这个 output，应用因此认为自己是"可见的"；
- **不做持续合成**：
  - Hidden 状态的 Stage 只需按固定节拍（默认 30Hz，可配置）回复 `wl_surface.frame`，保证应用不降帧、不暂停；
  - 只有当 AI 调用 `observe` 或需要显示缩略图时，才把主窗口 + 所有 `xdg_popup` + 子 surface 合成为一张以主窗口为原点的图像；
- 渲染器默认使用 **Pixman 软件渲染**：WSL2 内没有 DRM render node，GBM 不可用，GLES 渲染能否工作【待验证】。应用自身的渲染（含 GPU 加速）不受影响；
- 每次合成产生递增的 `frame_seq`，供 AI 接口做过期检测。

### 4.4 DisplaySink（把 Stage 显示到 Win11 桌面）

统一接口：`show(stage, rect)`、`hide(stage)`、`present(stage, damage)`、输入事件回调。

**WslgSink（第一版）**

- `focusstaged` 同时作为 WSLg 的客户端：每个 Desktop 状态 Stage 的每个顶层窗口，在 WSLg 中对应一个**代理窗口**；WSLg 负责把代理窗口变成 Win11 原生窗口（任务栏、拖动、缩放、Alt-Tab 都由它提供）；
- 收入后台 = unmap 代理窗口；恢复 = 按 `SavedRect` 重新 map；
- 代理窗口上的鼠标键盘事件转交 Router，作为人工输入；
- 用户拖动缩放代理窗口时，向应用发送 `configure` 让其重新排版，而不是拉伸图像；
- 代理窗口走 **X11 协议**连接 WSLg 的 Xwayland，因为 Wayland toplevel 无法指定屏幕坐标。【待验证 · M2】WSLg 的 RAIL 是否遵循 X11 窗口的指定位置。

**HostSink（备选，仅当 WslgSink 不够用时实现）**

- Win11 原生 .NET 壳 + hvsocket（AF_VSOCK）传输增量帧与输入；
- 帧按脏区增量 + LZ4 压缩，只给 Desktop 状态或正在被观看的 Stage 推帧；
- 触发条件：WSLg 无法原位恢复窗口，或输入法 / DPI / 延迟体验无法接受。

**DrmSink（远期）**

- DRM/KMS + libinput 后端，直接接管物理显示器与键鼠，脱离 Windows 独立开机。

### 4.5 Clipboard

- 每个 Stage 持有**自己的一份选区**（clipboard + primary）；合成器在向某个 Stage 投递焦点时，只向它提供它自己的选区；
- 默认策略：
  - Human 状态的 Stage 与 Windows 剪贴板**双向同步**（日常使用手感一致）；
  - Agent 状态的 Stage **完全隔离**，AI 的复制粘贴不会污染 Windows 剪贴板，也不会串到别的 Stage；
- AI 可通过 MCP 显式读写自己 Stage 的剪贴板；
- 不依赖多 `wl_seat`：选区由合成器在各 Stage 之间转发，本来就由合成器掌控。

### 4.6 A11y（无障碍树）

- 读取 AT-SPI 会话总线，按 PID 把可访问对象归属到 Stage；
- 为 AI 提供结构化的界面信息（控件角色、名称、文字、边界框），比纯截图定位更可靠；
- Chromium/Electron 需加 `--force-renderer-accessibility`，由 Stage Manager 在启动时自动附加。

### 4.7 文本输入

- **AI 输入文字**：不需要输入法。优先通过 `zwp_text_input_v3` 直接 commit 字符串；不支持的应用（含 X11）回退为临时上传含目标字符 keysym 的键位映射再模拟按键；
- **用户输入中文**：
  - 第一阶段：在 WSL 内运行 `Fcitx5`，候选框作为 Stage 的子图层合成；
  - 后续：把 Win11 输入法的提交结果直接转成 `text_input_v3` 的 preedit/commit，实现外层统一输入法。

### 4.8 文件与外部集成

- 引导应用的默认目录（XDG User Dirs）指向 `/mnt/c/Users/<user>/` 下的桌面、下载等目录；
- 已知限制：`/mnt/c` 走 9P 文件系统，大量小文件读写（如 VS Code 打开大仓库）明显变慢；对性能敏感的项目建议放在 WSL 原生文件系统中；
- 从资源管理器拖文件到代理窗口：路径 `C:\...` 转为 `/mnt/c/...`，经 `wl_data_device` 投递拖放；
- 链接打开 / "在文件夹中显示"：按策略交给托管浏览器或 Win11 原生程序。

---

## 5. AI 控制接口（MCP）

Windows 上的 Agent 以 **stdio** 方式连接：MCP 配置中把命令设为 `wsl.exe -e focusstage mcp` 即可，不需要网络端口，也不涉及跨 VM 的 socket。

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

`actions[]` 支持：`move`、`click`、`double_click`、`drag`、`scroll`、`key`（含组合键）、`type_text`、`wait(ms)`。

### 5.2 语义约定

- **坐标**：一律使用 Stage 主窗口左上角为原点的逻辑像素，与桌面位置、DPI 无关；
- **租约**：`acquire` 失败时返回当前持有者；租约到期未续自动释放；
- **人永远优先**：用户接管后租约立即作废，之后的 `act` 返回 `preempted`，正在执行的动作序列中止；
- **过期检测**：`act` 可携带 `based_on`，如果 Stage 画面在该帧之后已发生变化，返回 `stale`，避免 AI 对着旧画面点错；
- **原子性**：一次 `act` 中的动作按顺序执行，任一步失败即中止，并返回已执行的步数。

---

## 6. 实施路线

每个里程碑都有明确的验收标准与「不过就停」判据。

### M0：多焦点隔离验证（2~3 天）

- 范围：在 WSL2 中用 Rust + Smithay 写一个无头合成器（Pixman 渲染），不接入任何 Windows 显示。
- 验收：
  - 同时托管 1 个 Chromium（Wayland）、1 个 `foot` 终端、1 个 X11 应用（rootless Xwayland）；
  - 脚本**同时**向三者持续输入文字，三者都正确收到且互不串扰；
  - 分别对三个 Stage 截图，内容正确，右键菜单 / 下拉框能出现在截图中。
- **不过就停**：若"永远前台"在 Chromium 或 X11 上无法稳定工作，暂停后续开发，回到第 2 章重新设计隔离方式。

### M1：AI 后台并发可用

- 范围：Control API + MCP 服务（`wsl.exe` stdio）+ 租约 + A11y + 调试用的 Stage 画面查看器。
- 验收：Windows 上的 AI Agent 同时操作两个后台 Stage（如浏览器填表 + 编辑器写文字），用户在 Win11 上正常工作，**零焦点丢失、零鼠标抖动**。
- 意义：**到这一步 AI 并发的核心价值已经可用**，尚未涉及桌面显示。

### M2：桌面三态闭环

- 范围：WslgSink + Tray（全局接管热键、Stage 列表）。
- 验收：用户在桌面窗口里打字 → 一键收入后台 → AI 继续操作 → 按热键接管 → 窗口在原位置、原大小出现，内容包含 AI 的全部修改。
- **不过就停**：若 WSLg 无法原位恢复窗口，或输入体验无法接受，转为实现 HostSink。

### M3：日常手感

- 剪贴板与 Windows 双向同步、中文输入（Fcitx5）、高 DPI 缩放、文件拖放、链接路由、窗口缩放时的重新排版。

### M4 / M5：按需扩展

- M4：HostSink（仅当 M2 判定 WslgSink 不够用）。
- M5：DrmSink，脱离 Windows 独立开机。

---

## 7. 风险与应对

| 风险 | 影响 | 应对 | 验证时机 |
| :--- | :--- | :--- | :--- |
| 应用在"永远前台"状态下行为异常 | 核心方案不成立 | M0 覆盖 Chromium / Electron / GTK / Qt / X11 各一个代表 | M0 |
| 应用离屏后降帧或暂停 | AI 看到的画面停滞 | 虚拟 output + 固定节拍 frame callback，始终保持 `activated` | M0 |
| 弹出菜单不在截图中或坐标错位 | AI 点不到下拉项 | 截图时按 surface 树合成主窗口 + popup + 子 surface | M0 |
| 一个 Stage 被多个进程共享（如浏览器复用已有进程） | 虚拟焦点产生矛盾 | 强制独立 profile / 独立 Xwayland，Stage Manager 检测并拒绝 | M0 |
| GLES 渲染器在 WSL2 中不可用 | 无法合成截图 | 默认 Pixman 软件渲染 | M0 |
| WSLg 不遵循 X11 窗口坐标 | 无法原位恢复 | 转 HostSink | M2 |
| 每个 X11 Stage 一个 Xwayland 带来内存开销 | 同时托管数量受限 | 每实例约数十 MB，可接受；空闲 Stage 可挂起 | M3 |
| `/mnt/c` 9P 性能差 | 开发工具卡顿 | 文档说明，大仓库放 WSL 原生文件系统 | M3 |
| AI 可读写真实用户目录 | 误删误改文件 | 后续为 Stage 增加可选沙箱（bubblewrap，目录白名单 / 只读挂载） | M3 之后 |

---

## 8. 明确不做的事

- **不按窗口分配 `wl_seat`**：依赖客户端的多 Seat 支持，兼容性没有保证。
- **不在早期自研传帧与输入转发**：WSLg 已经提供了把 Linux 窗口变成 Win11 窗口的能力。
- **不做持续离屏合成**：只在 AI 观察或需要缩略图时合成。
- **Windows 侧不承载业务逻辑**：Tray 只负责热键与入口，所有状态集中在 `focusstaged`。
- **不接管已在 Win11 原生运行的 `.exe`**：纯 Windows 软件仍由 MarkScreen 等外层方案处理。

---

## 9. 与 MarkScreen 的关系

- **理念继承**：沿用 MarkScreen 的视觉定位与"人随时可打断接管"原则，但把执行载体从"在全局物理桌面上抢鼠标键盘"升级为"在合成器里按 Stage 隔离输入"。
- **分工**：MarkScreen 继续处理少量只能在 Windows 上运行的原生软件；FocusStage 承载浏览器与跨平台客户端的后台并发操作。
- **定位说明**：纯浏览器任务用 CDP / Playwright 已经可以并发自动化；FocusStage 的主要价值在于**非浏览器桌面客户端**，以及"人和 AI 共用同一个正在使用的窗口"这一场景。
