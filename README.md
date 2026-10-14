# Docker Pull Tar

Docker Pull Tar 是一个面向 Windows 10/11 的免安装桌面客户端。它无需本机 Docker 环境，即可从 Docker/OCI Registry 下载指定平台的镜像并导出为可由 `docker load` 导入的 tar 文件。

## 当前架构

- `src/`：Vue 3 + TypeScript 界面、Pinia 状态和 Tauri 调用封装。
- `src-tauri/`：Tauri 2 桌面壳及 Rust 核心。
- `src/api/tauri.ts`：唯一的界面/Rust 通信 seam，集中封装 commands 与 events。
- Rust 模块负责 Registry 鉴权、清单解析、分层下载、断点续传、摘要校验、归档、设置与历史。
- Registry 密码保存在 Windows Credential Manager，不写入配置 JSON。

应用不会启动 Python 后端、Electron 进程、localhost API 或 WebSocket bridge，也不会执行任意 shell 命令作为界面与 Rust 的通信方式。

```text
Vue views / Pinia stores
          |
          v
src/api/tauri.ts (typed invoke + events)
          |
          v
Rust commands -> task_manager -> registry / blob / archive
                          |
                          +----> storage / credentials / history
```

## 主要功能

- 新增、编辑、删除和设置默认镜像源。
- 为每个私有 Registry 保存独立认证信息。
- 支持 amd64、arm64 和 arm/v7。
- 支持流式并发下载、协作式取消、断点续传和 SHA-256 校验。
- 生成 Docker 兼容归档，并记录成功、失败与取消历史。
- 使用带 revision 的恢复快照保护设置与历史一致性。

## 环境要求

- Windows 10/11 x64。
- Microsoft Edge WebView2 Runtime。
- Node.js 20+ 与 pnpm 9。
- Rust 1.85+，包含 `x86_64-pc-windows-msvc` target。
- Visual Studio 2022 Build Tools，包含 Desktop development with C++ 和 Windows SDK。

运行已经构建的 `DockerPull.exe` 不需要 Node.js、Rust、Python 或 Docker。

## 开发启动

所有命令都在仓库根目录执行：

```powershell
pnpm install
pnpm dev
```

`pnpm dev` 启动 Vite 热更新和 Tauri 桌面窗口，不需要单独启动后端进程。

## 常用命令

| 命令 | 用途 |
| --- | --- |
| `pnpm dev` | 启动 Tauri 开发客户端 |
| `pnpm dev:web` | 仅启动 Vite 开发页面 |
| `pnpm test` | 运行界面测试 |
| `pnpm typecheck` | 检查 Vue/TypeScript 类型 |
| `pnpm build:web` | 构建 Vue 静态资源 |
| `pnpm build:tauri` | 构建当前平台的 Tauri release |
| `pnpm build:portable` | 构建 Windows x64 免安装版 |

Rust 模块维护命令：

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --all-targets
```

## 便携版

```powershell
pnpm build:portable
```

脚本先扫描运行时代码中的旧架构引用，然后构建 `x86_64-pc-windows-msvc` release，并输出：

```text
release/portable/DockerPull.exe
```

脚本同时打印文件大小和 SHA-256。便携程序依赖 Windows WebView2 Runtime；用户配置保存在 Windows 标准应用配置目录，凭据由 Windows Credential Manager 管理。

## 使用

1. 输入镜像名称，例如 `nginx:latest` 或 `registry.example.com/team/app:v2`。
2. 选择镜像源、目标架构和输出目录。
3. 开始下载，等待下载、校验和打包完成。
4. 在目标环境执行 `docker load -i .\image.tar`。

## 目录结构

```text
docker-pull-tar/
|-- src/                     # Vue 界面、状态与 Tauri 调用封装
|-- src-tauri/
|   |-- src/                 # Rust commands 和领域模块
|   |-- tests/               # Rust 集成测试
|   |-- capabilities/        # Tauri 最小权限配置
|   `-- tauri.conf.json
|-- resources/               # Windows 应用图标
|-- scripts/
|   `-- build-portable.ps1
|-- docs/superpowers/        # 架构设计与迁移历史
|-- CONTEXT.md               # 领域统一语言
|-- AGENTS.md                # AI 代理约束
|-- index.html
|-- vite.config.mts
|-- tsconfig.json
`-- package.json
```

详细设计见：

- `docs/superpowers/specs/2026-07-12-tauri-rust-windows-client-design.md`
- `docs/superpowers/specs/2026-07-18-config-revision-recovery-design.md`

## AI 协作约束

所有 AI 代理必须遵守根目录 `AGENTS.md`。未经用户明确、直接许可，严禁执行 Git 提交或推送。
