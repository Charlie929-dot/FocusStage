# FocusStage 中 T3 Code 卡顿与启动故障修复报告

2026 年 11 月 24 日 · Windows 11 + Ubuntu WSL2 · T3 Code Nightly

T3 Code 已从原来的 Windows 开始菜单快捷方式恢复启动。正式安装使用 XWayland，已用默认 T3 数据目录打开主界面和设置页。隔离环境中的同一动效对照，应用帧回调由 13.1 次/秒升至约 60 次/秒，Windows 宿主收帧确认由 10.8 次/秒升至约 57–59 次/秒。

## 卡顿原因

- **Electron 使用了软件合成。** 修复前，T3 Code 在 FocusStage 的 Wayland 路径上使用 Mesa `llvmpipe`。给 Wayland 显式选择 D3D12 后，GPU 合成仍显示为 `disabled_software`。切到 XWayland 后，DevTools 确认 ANGLE 通过 D3D12 使用 Intel Iris Xe，GPU 合成、光栅化和 WebGL 均启用。
- **跨 WSL 的像素传输占用了大部分帧时间。** 每张 1080p BGRA 原始帧约 7.91 MiB。一次旧路径测试中，发送至宿主确认平均耗时 19.29 ms，其中管道写入和刷新占 18.81 ms。此前客户端准备下一帧还会等宿主确认，传输耗时也拖慢了应用侧更新。

## 修复内容

| 改动 | 实现 |
| --- | --- |
| WSL2 图形后端 | Electron 和 Chromium 默认走 FocusStage 内置 XWayland。保留显式 `--ozone-platform` 参数。存在 `/dev/dxg` 时默认使用 Mesa D3D12，保留现有环境变量覆盖。 [启动逻辑](compositor/src/manager.rs#L323) |
| 分数 DPI | 在 XWayland 启动时设置客户端比例和 `Xft/DPI`，修正 Smithay 的窗口坐标转换及输出比例通告。125% 缩放下，1280×800 的逻辑窗口对应 1600×1000 的像素缓冲，T3 Code 和 VS Code 均测得 `devicePixelRatio = 1.25`。 [XWayland 初始化](compositor/src/xwayland.rs#L329) · [Smithay 输出比例](third_party/smithay/src/wayland/output/mod.rs#L164) · [窗口坐标转换](third_party/smithay/src/xwayland/xwm/mod.rs#L1644) |
| 原生帧传输 | Windows 宿主在握手中声明 LZ4 支持。达到 256 KiB 且压缩至少节约八分之一的帧使用 LZ4，其余帧及旧宿主继续接收原始 BGRA。解压在 Windows 读取线程完成，并校验解压后的长度。 [发送端](compositor/src/sink/native.rs#L289) · [Windows 宿主](tray/NativeHost.cs#L350) |
| 帧调度和显示 | Wayland 帧回调由合成器定时器驱动，应用可以在传输期间准备下一帧。每窗口仍由 `frame_done` 限制为最多一帧在途，Windows 端使用 Direct2D 的立即呈现选项。 [合成器](compositor/src/main.rs#L355) · [宿主确认](compositor/src/sink/native.rs#L477) · [Direct2D](tray/NativeHostRenderer.cs#L161) |

## 性能对照

在同一台机器上，以 125% 缩放、1280×800 逻辑窗口运行 T3 Code。使用隔离配置，在应用界面上运行同一段约 2.2 秒的动效。滚轮另行通过 Windows 原生窗口和 MCP 输入实测。

| 配置 | 应用 `requestAnimationFrame` 频率 | 应用帧间隔 P95 | 宿主确认频率 |
| --- | ---: | ---: | ---: |
| 修复前，默认 Wayland | 13.1 次/秒 | 153.0 ms | 10.8 次/秒 |
| 修复后，默认 XWayland | 60.0 次/秒 | 16.8 ms | 56.7–58.9 次/秒 |
| 修复后，显式 Wayland | 13.4 次/秒 | 99.0 ms | 12.0 次/秒 |

应用帧回调频率衡量浏览器调度。宿主确认频率衡量 Windows 收下像素帧的速度。显示器上的实际呈现间隔未测量。

另用同一张可压缩的 1080p 测试帧测量传输。原始负载为 8,294,400 字节，两轮发送至确认分别平均 22.33 ms 和 43.43 ms。LZ4 负载为 32,783 字节，两轮分别平均 3.09 ms 和 3.12 ms。压缩样本预先生成，这组数字反映传输和宿主接收解压的耗时。实际 T3 Code 界面动效的收益以上表为准。

## 正式安装与启动恢复

1. Rust 发布构建完成，工作区 44 个测试通过。Windows `dotnet publish` 完成。安装期间发现发布文件落入桌面会话的重定向目录，WSL 无法从登记路径访问。将缺失或不同的发布文件补齐到正式目录后重启托盘，确认正式 daemon 报告 `desktop_active: true`，隔离配置下的 T3 Code 启用了 XWayland 和 D3D12。
2. 首次安装验证只覆盖了隔离配置，过早判断原快捷方式已可用。实际点击原快捷方式后，T3 Code 报出 `This T3 home is unavailable`，指出默认数据目录正被 PID `48769` 占用。进程启动时间、会话记录和系统服务范围表明，这是显卡对照测试退出后留下的后台进程，仍监听本机 `3773` 端口。
3. 确认没有已建立的客户端连接后，向该测试进程发送 `SIGTERM`，确认进程退出且端口释放。随后关闭报错窗口，再次打开原快捷方式。新进程打开了 `~/.t3/userdata/statev2.sqlite`，主界面加载成功，设置页可点击打开。窗口留在桌面，原有数据目录保留。

## 验证范围

- 实测 T3 Code 设置页的 Windows 原生点击和滚轮，以及 MCP 在窗口可见和隐藏时的滚轮。验证了 Windows 宿主的原始帧兼容路径。
- VS Code 验证了默认 XWayland、D3D12 GPU 合成和 125% DPI。Snap 版 Chromium 在 FocusStage XWayland 和 WSLg 中都出现过 GLX/EGL 初始化失败并回退到软件渲染。加速数字适用于本机的 T3 Code 对照场景。
- XWayland 的 MCP 文本输入通过键位模拟，窗口比例使用启动时的 DPI。
