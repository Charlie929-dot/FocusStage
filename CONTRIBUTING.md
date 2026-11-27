# 参与贡献 / Contributing to FocusStage

感谢你对 FocusStage 的兴趣！欢迎通过 Issue 反馈问题、提出建议或提交 Pull Request。

## 开发环境准备

- **Windows 11（64 位）**：
  - [.NET 8 SDK](https://dotnet.microsoft.com/download/dotnet/8.0)
  - Git
  - （可选）Inno Setup 6（仅本地打 `.exe` 安装包时需要）
- **WSL2 + Ubuntu（推荐 22.04 / 24.04 / 26.04 LTS）**：
  - [Rust 工具链](https://rustup.rs/)（stable）
  - 原生编译依赖：
    ```bash
    sudo apt-get update
    sudo apt-get install -y pkg-config libwayland-dev libpixman-1-dev libxkbcommon-dev
    ```

## 构建、测试与本地部署

1. **运行 Rust 单元测试（在 WSL 内）**：
   ```bash
   cargo test --workspace
   ```
2. **构建 Windows 启动器与宿主（在 Windows PowerShell 内）**：
   ```powershell
   dotnet build tray\FocusStageTray.csproj -c Release --nologo
   ```
3. **一键部署到本机验证**：
   - 从 WSL 终端运行：
     ```bash
     ./scripts/install.sh
     ```
   - 或从 Windows PowerShell 运行：
     ```powershell
     powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install.ps1
     ```

## 设计与代码规范

在提交改动前，建议先阅读 [DESIGN.md](DESIGN.md) 了解整体架构边界：

- **通用机制优先**：尽量从协议、合成器或会话层解决根因，避免为单个应用堆砌脆弱特判。
- **核心逻辑留在 WSL**：Stage 状态机、路由、租约与桌面集成集中在 `compositor/` 与 `mcp/`；`tray/` 保持为交互与呈现壳。
- **补充测试与文档**：涉及配置项、CLI、协议或环境变量优先级的改动，请补充对应的单元测试并同步更新 `README.md` / `DESIGN.md`。

## 提交 Pull Request

1. 从最新的 `master` 分支切出特性或修复分支。
2. 确保 `cargo test --workspace` 与 `dotnet build tray/FocusStageTray.csproj -c Release` 均通过。
3. 保持提交记录清晰，PR 描述中说明改动背景、方案及实测范围。
