# Docker Pull Tar Tauri/Rust Windows 客户端详细设计

## 1. 文档目的

本文档记录 Docker Pull Tar 2.0 的当前实现、功能完成度、已知缺陷，以及将 Electron + Python/FastAPI 重构为 Vue 3 + Tauri 2 + Rust 的目标架构。它既是现状交接基线，也是后续架构调整和实施验收依据。

本文档描述的最终交付物是 Windows x64 免安装便携版 `DockerPull.exe`：

- 不依赖本机 Python、Docker、Node.js 或独立后端服务。
- 不监听本机 HTTP 端口。
- 使用 Windows WebView2 渲染现有 Vue 3 界面。
- 使用 Rust 实现镜像仓库访问、下载、校验、归档、配置和历史记录。
- 用户配置保存在 `%APPDATA%\docker-pull-tar\config.json`，下载文件写入用户选择的目录。

## 2. 项目现状

### 2.1 技术构成

| 层次 | 当前技术 | 主要职责 |
| --- | --- | --- |
| 桌面壳 | Electron 30 | 窗口、菜单、目录选择、打开目录、Python 子进程管理 |
| 前端 | Vue 3、Pinia、Vue Router、Element Plus、Axios | 下载、历史、设置界面和内存状态 |
| 后端接口 | FastAPI、Uvicorn、WebSocket | REST API、日志和进度广播 |
| 下载核心 | Python、requests、ThreadPoolExecutor | Registry V2 认证、Manifest 获取、Layer 并发下载、tar 组装 |
| 持久化 | 单个 JSON 文件 | 设置、镜像源、认证信息、最多 100 条历史记录 |
| 打包 | Vite、electron-builder、PyInstaller | Electron 便携包和 Python `backend.exe` |

当前打包目录中，PyInstaller 生成的 `backend.exe` 约 22.98 MB；Electron 分发还包含 Chromium/Node 运行时。当前架构同时承担两套运行时和双进程生命周期成本。

### 2.2 当前目录职责

```text
backend/
  main.py                         FastAPI 入口、CORS、WebSocket
  models/schemas.py               Pydantic 请求、设置、历史模型
  routers/download.py             单下载任务 API 和内存状态
  routers/settings.py             设置与镜像源 API
  routers/history.py              历史记录 API
  services/docker_pull.py         下载、解压、归档的主要实现
  services/log_websocket.py       日志与进度广播
  services/settings_store.py      config.json 读写
frontend/
  src/api/index.ts                Axios REST 客户端
  src/stores/download.ts          下载状态、WebSocket、轮询
  src/stores/settings.ts          设置和镜像源状态
  src/views/                       下载、历史、设置页面
  electron/main.ts                Electron 生命周期和 IPC
  electron/pythonProcess.ts       Python/Conda/EXE 启停
backend.spec                      PyInstaller 配置
startup.config.json              开发期 Python 启动模式
```

### 2.3 当前运行链路

1. Electron 启动 Python 源码、Conda 环境或打包后的 `backend.exe`。
2. Vue 固定连接 `http://127.0.0.1:8000` 和 `ws://127.0.0.1:8000/ws/logs`。
3. 前端发起下载后，FastAPI 创建一个后台 `asyncio` 任务。
4. Python 获取 Registry Bearer Token 和 Manifest，选择目标 Linux 架构。
5. Python 线程池并发下载各 Layer，通过 WebSocket 广播日志和进度。
6. Python 解压 gzip Layer，生成旧式 Docker image layout，再打包为可供 `docker load` 使用的 tar。
7. 成功后把历史记录追加到 `%APPDATA%\docker-pull-tar\config.json`。

## 3. 当前完成度

### 3.1 已实现

| 能力 | 完成度 | 代码现状 |
| --- | --- | --- |
| Windows 桌面界面 | 已实现 | Electron 固定 1000x700 窗口，含下载、历史、设置三页 |
| 公有 Registry 下载 | 已实现 | 支持 Docker Registry V2 Bearer Token 流程 |
| 私有 Registry 基础认证 | 已实现但安全性不足 | 用户名和密码随镜像源保存，并用于 Token 请求 |
| 镜像名解析 | 基本实现 | 支持短名称、命名空间、Registry 域名和端口、Tag |
| 多架构选择 | 基本实现 | 从 Manifest List/OCI Index 中选择 Linux `amd64`、`arm64` 或 `armv7` |
| Layer 并发下载 | 已实现 | 线程数可配置，Pydantic 接受 1-16，界面限制 1-8 |
| 实时日志与进度 | 已实现但状态不可靠 | WebSocket 广播日志和逐层进度，前端具备有限重连 |
| 取消下载 | 部分实现 | 设置共享停止事件并取消外层异步任务 |
| docker-load tar 生成 | 基本实现 | 生成 config、manifest.json、repositories 和 layer.tar |
| 设置持久化 | 已实现但并发不安全 | 单 JSON 文件保存设置和镜像源 |
| 下载历史 | 部分实现 | 仅成功任务写入，最多保留 100 条 |
| 历史重新下载 | 已实现 | 将历史参数带回下载页，失效镜像源回退到默认源 |
| 输出目录选择/打开 | 已实现 | Electron IPC 调用系统目录对话框和资源管理器 |
| Python 后端打包 | 已实现 | PyInstaller 生成约 23 MB 的独立 EXE |
| Electron 便携包 | 已实现 | electron-builder `portable` 目标 |

### 3.2 未完成或与声明不一致

| 能力 | 当前问题 |
| --- | --- |
| 断点续传 | README 声明支持，但代码未发送 `Range`，每次使用 `wb` 覆盖 Layer 文件 |
| Digest 完整性 | 代码计算 Layer SHA-256，但未与 Manifest Digest 比较 |
| TLS 安全 | 所有 Registry 请求均使用 `verify=False`，并全局关闭告警 |
| 状态查询降级 | 路由的 `download_state.progress` 从未由下载服务更新，REST 轮询无法返回真实进度 |
| WebSocket 恢复 | 重连后没有状态快照或日志游标，断线期间信息永久丢失 |
| 任务终态 | 失败和取消不写历史；取消可能被外层任务取消截断清理逻辑 |
| 临时文件策略 | 失败、取消和进程退出后没有一致的恢复或清理规则 |
| 配置并发 | 多个 `SettingsStore` 实例各自加载、修改并全量覆盖同一 JSON 文件，存在丢更新风险 |
| 部分更新语义 | FastAPI 接收完整 `Settings` 模型，前端声称发送 `Partial`；缺失字段可能被 Pydantic 默认值覆盖 |
| 默认镜像一致性 | 模型、下载路由和前端回退值在 `registry-1.docker.io` 与 `docker.1ms.run` 之间不一致 |
| OCI 压缩格式 | 代码无条件用 gzip 解压 Layer，不支持未压缩层或其他明确 mediaType |
| Manifest 平台选择 | 只比较 `architecture` 与 `os`，未处理 ARM `variant` 等平台细节 |
| 错误响应 | “已有任务”使用 HTTP 200 + `{error}`，前端仍可能将状态置为 downloading |
| 密码存储 | 私有仓库密码明文写入 config.json |
| 前端响应性 | `DownloadProgress.vue` 在 setup 时解构普通值，部分状态可能不会随 Pinia 更新 |
| 测试 | 仓库没有 Python、TypeScript、组件、集成或打包自动化测试 |

## 4. 重构目标与非目标

### 4.1 目标

1. 用 Tauri 2 替换 Electron，用 Rust 完整替换 Python/FastAPI 下载后端。
2. 保留 Vue 3、Pinia、Vue Router、Element Plus 和现有主要交互流程。
3. 删除本机 REST、WebSocket、CORS、端口探测和子进程管理。
4. 实现真正可验证的断点续传、Digest 校验、安全 TLS 和一致取消。
5. 建立单一任务状态源，使命令查询和事件通知始终一致。
6. 保持读取现有 `config.json` 的兼容性，并以原子写入避免配置损坏或丢更新。
7. 输出 Docker 可导入的 tar，并通过 fixture 和可选的 `docker load` 验证。
8. 生成 Windows x64 免安装 `DockerPull.exe`，不生成 MSI/NSIS。

### 4.2 非目标

- 首版仍只支持同时运行一个镜像下载任务，不引入下载队列。
- 不实现 Registry Push、镜像构建、容器运行或 Docker daemon 管理。
- 不把应用改造成远程服务或多用户系统。
- 不在首版大规模改版 Vue 页面视觉设计。
- 不承诺绕过企业自签名证书；若以后需要，应增加显式证书导入，而不是关闭 TLS 校验。
- 首版只要求 Windows x64，其他桌面平台以后按 Rust 核心可移植性扩展。

## 5. 目标架构

```text
DockerPull.exe
├─ Windows WebView2
│  └─ Vue 3 + Pinia
│     ├─ invoke(command, payload)
│     └─ listen(download://*)
└─ Tauri/Rust Core
   ├─ commands       UI 可调用的稳定边界
   ├─ task_manager   单任务状态机、取消令牌、快照和事件
   ├─ registry       镜像解析、认证、Manifest、Blob 下载
   ├─ archive        Digest 校验、Layer 解压、Docker tar 组装
   ├─ storage        配置迁移、原子持久化、历史记录
   └─ error          稳定错误码和用户可读消息
```

### 5.1 模块职责

#### `commands`

Tauri command 是唯一 UI 后端接口。它只负责参数校验、调用领域服务和把领域错误转换为可序列化错误，不包含下载算法。

#### `task_manager`

持有唯一活动任务和最新 `DownloadSnapshot`。它负责：

- 原子检查“是否已有活动任务”。
- 创建任务 ID 和 `CancellationToken`。
- 驱动状态机。
- 保存最新进度并发出 Tauri events。
- 确保完成、失败、取消三个终态只提交一次。
- 应用退出时发出取消并等待清理窗口；超时后保留可恢复临时文件。

#### `registry`

负责 Docker Registry V2/OCI Distribution 交互：

- 解析镜像引用并标准化 registry、repository、tag。
- 对 `/v2/` 发起 challenge 请求，解析 `WWW-Authenticate`。
- 支持匿名 Token 和带 Basic Credentials 的 Token。
- 获取 Docker v2 Manifest、Manifest List、OCI Manifest、OCI Index。
- 按 `os=linux`、`architecture` 和可选 `variant` 精确选择平台。
- 获取 config 和 blobs，支持重试、限时、取消、续传和摘要校验。

HTTP 客户端使用 `reqwest` + Rustls，默认启用系统可信证书校验。认证头和密码禁止写入日志。

#### `archive`

负责把已验证内容转换为 Docker 可加载 tar：

- 识别 gzip 和未压缩 Layer mediaType。
- 对每个下载 Blob 强制验证 `sha256`。
- 生成稳定的 Layer ID、parent 链、`json`、`manifest.json` 和 `repositories`。
- 在输出目录同卷创建临时 tar，完成后原子重命名为最终文件。
- 打包成功前不得覆盖已有同名有效 tar；冲突时生成不重复名称或返回明确错误。

#### `storage`

进程内只存在一个共享实例，使用异步读写锁保护内存数据，并由事务互斥锁串行化所有配置 revision。配置采用 `config.json` 与 `config.json.recovery` 双快照协议；每个已提交快照包含单调递增 `revision` 和唯一 `transactionId`。完整恢复快照写入、flush、`sync_all`、原子替换 recovery 路径，并完成替换后的目录持久化屏障（Windows 使用 write-through 替换语义）后才达到提交点；主文件提升失败不得回滚已提交状态，也不得提前消耗唯一 recovery 副本。

启动时独立验证主文件和恢复文件，选择 revision 更高的有效快照。相同 revision 但 transaction 或内容冲突时返回 `CONFIG_CORRUPTED`，不得按文件时间或路径优先级猜测。旧 `version=1.0` 与当前 `version=2.0` 配置迁移到 version 3.0，并保留合法设置、镜像源、凭据引用、待清理凭据和历史。

旧凭据删除采用 pending-first 顺序：移除活动引用的同一 revision 必须先把旧引用写入 `pendingCredentialCleanup`，随后才执行幂等删除；确认删除后再提交清除 pending 的 revision。密码始终只存放于 Windows Credential Manager。

完整提交、启动选择、迁移、错误和故障注入矩阵见 [Revisioned Config Recovery Design](./2026-07-18-config-revision-recovery-design.md)。

### 5.2 依赖方向

```text
Vue -> commands -> task_manager -> registry
                           ├-----> archive
                           └-----> storage

registry/archive/storage -> domain models + AppError
```

领域模块不得依赖 Vue 或 Tauri 窗口对象。事件发送通过小型 `EventSink` trait 注入，以便核心逻辑在无 GUI 环境中测试。

## 6. 前后端契约

### 6.1 Commands

| Command | 输入 | 输出 |
| --- | --- | --- |
| `download_start` | `DownloadStartRequest` | `DownloadSnapshot` |
| `download_cancel` | 无 | `DownloadSnapshot` |
| `download_status` | 无 | `DownloadSnapshot` |
| `settings_get` | 无 | `AppSettings` |
| `settings_update` | `SettingsPatch` | `AppSettings` |
| `mirrors_list` | 无 | `Mirror[]` |
| `mirror_add` | `MirrorInput` | `Mirror` |
| `mirror_update` | `id`, `MirrorPatch` | `Mirror` |
| `mirror_delete` | `id` | `CommandResult` |
| `mirror_set_default` | `id` | `AppSettings` |
| `history_list` | 无 | `HistoryItem[]` |
| `history_delete` | `id` | `CommandResult` |
| `history_clear` | 无 | `CommandResult` |
| `dialog_select_directory` | 可选初始目录 | `string | null` |
| `path_open_parent` | 文件路径 | `CommandResult` |

Rust 对外字段统一使用 TypeScript 当前的 camelCase，通过 Serde rename 规则生成。`SettingsPatch` 和 `MirrorPatch` 使用 `Option` 表达真正的部分更新，不允许默认值隐式覆盖未提交字段。

### 6.2 Events

| Event | Payload | 用途 |
| --- | --- | --- |
| `download://state` | `DownloadSnapshot` | 每次状态转换和最终状态 |
| `download://progress` | `DownloadProgress` | 节流后的总体和逐层进度，默认最多每 250 ms 一次 |
| `download://log` | `LogEvent` | 用户可读日志，不含认证信息 |

监听建立后，前端必须立即调用 `download_status` 获取快照；事件只负责增量通知，快照负责断线、页面切换或窗口恢复后的收敛。前端不再根据 Layer 是否全部完成自行推导任务终态。

### 6.3 错误模型

所有 command 错误序列化为：

```ts
interface CommandError {
  code: string
  message: string
  retryable: boolean
  details?: Record<string, string>
}
```

稳定错误码至少包括：

- `TASK_ALREADY_RUNNING`
- `INVALID_IMAGE_REFERENCE`
- `REGISTRY_AUTH_FAILED`
- `REGISTRY_NOT_FOUND`
- `PLATFORM_NOT_FOUND`
- `TLS_VALIDATION_FAILED`
- `NETWORK_TIMEOUT`
- `RESUME_NOT_SUPPORTED`
- `DIGEST_MISMATCH`
- `UNSUPPORTED_LAYER_MEDIA_TYPE`
- `OUTPUT_NOT_WRITABLE`
- `DISK_SPACE_INSUFFICIENT`
- `TASK_CANCELLED`
- `CONFIG_CORRUPTED`
- `INTERNAL_ERROR`

日志呈现上下文，错误码驱动 UI 行为。可重试错误保留 `.part`，不可重试的内容错误隔离或删除损坏部分。

## 7. 下载状态机

状态扩展为：

```text
idle -> preparing -> downloading -> verifying -> packing -> completed
                    |             |            |
                    +-------------+------------+-> failed
                    +-------------+------------+-> cancelled
```

规则：

- 只有 `idle`、`completed`、`failed`、`cancelled` 可以接受新任务。
- `download_start` 在返回前完成活动任务的原子占位，避免并发双启动。
- `completed` 只在最终 tar 原子提交并成功写历史后产生。
- 取消是协作式的：网络流、并发 worker、校验和打包循环均检查 token。
- 任意 panic 或未分类错误在任务边界转换成 `failed`，不得使 TaskManager 永久停留在活动状态。
- 新任务开始时创建新任务 ID；旧任务的迟到事件因 taskId 不匹配被前端丢弃。

`DownloadSnapshot` 包含 `taskId`、status、镜像信息、progress、startedAt、finishedAt 和最后错误。它是状态查询和状态事件的唯一数据源。

## 8. 断点续传与临时文件

每个任务使用输出目录下的隐藏工作目录：

```text
.<safe-image>-<tag>.docker-pull-tar/
  resume.json
  config.part
  blobs/
    sha256_<digest>.part
```

`resume.json` 记录 schema version、规范化镜像引用、平台、Manifest Digest、Blob Digest、期望长度、已下载长度和可用校验标识。

续传流程：

1. 只有镜像引用、平台和 Manifest Digest 全部匹配时才复用工作目录。
2. 已完整且 Digest 正确的 Blob 直接复用。
3. 对部分 Blob 发送 `Range: bytes=<length>-`；有 ETag 时同时发送 `If-Range`。
4. 收到 `206` 且 `Content-Range` 起点匹配时追加写入。
5. Registry 忽略 Range 返回 `200` 时清空该 `.part` 并从零下载。
6. `416` 时先按期望长度和 Digest 校验；有效则视为完成，否则从零下载。
7. 每个 Blob 完成后验证 Manifest 中的 SHA-256；不匹配时隔离损坏文件并返回 `DIGEST_MISMATCH`。
8. 取消、超时和可重试网络错误保留有效 `.part`；认证失败、Manifest 变化和不可支持格式不复用不匹配数据。
9. 最终 tar 成功提交后删除工作目录。

该策略不依赖服务端一定支持 Range，并避免把旧 Manifest 的 Layer 拼接到新镜像。

## 9. 并发、进度与资源控制

- `downloadWorkers` 首版统一限制为 1-8，默认 4。
- 使用 Tokio task + `Semaphore` 控制并发 Blob 数，不为每层创建 OS 线程。
- 每层维护 `waiting/downloading/verifying/completed/failed/cancelled` 状态。
- 总下载量包含启动时已存在且验证有效的续传字节。
- 速度采用短窗口采样，不把恢复字节计入当前网络速度。
- 事件聚合器最多每 250 ms 发一次进度，状态变化和层完成可立即发出。
- 下载数据流式写盘并增量计算 SHA-256，不把完整 Layer 载入内存。
- 打包前检查可用空间；估算至少覆盖剩余 Blob、解压层和最终 tar，并为元数据留出余量。

## 10. 配置、认证和历史

### 10.1 配置模型

新配置继续沿用现有字段，并增加 schema version。默认镜像源统一为 `registry-1.docker.io`；旧配置中用户选择的默认源保持不变。

密码兼容策略：首版迁移时仍能读取现有明文 `password`。写入新配置时使用 Windows Credential Manager 保存凭据，JSON 仅保存 credential reference；若凭据系统不可用，则明确提示用户且不静默回退到明文。该行为通过一个 `CredentialStore` trait 隔离，便于测试。

### 10.2 历史模型

所有终态均写历史：

- `completed`：记录最终 tar 路径和大小。
- `failed`：记录错误码和用户可读消息。
- `cancelled`：记录取消时间和可恢复工作目录存在与否。

历史默认按完成时间倒序返回，最多保留 100 条。历史删除不删除 tar 或断点文件；文件删除必须是未来独立、显式确认的能力。

## 11. 前端迁移

### 11.1 API 层

删除 Axios 客户端和 WebSocket 管理，新增 `src/api/tauri.ts` 封装 `@tauri-apps/api/core.invoke`。页面和 store 不直接拼 command 字符串。

### 11.2 下载 Store

- 使用 `storeToRefs` 或 computed 保持组件响应性。
- 初始化顺序为：注册 event listeners，再调用 `download_status`。
- 只接受当前 taskId 的事件。
- 不再从 Layer 列表推导 completed/failed。
- 页面切换不移除全局下载监听；只在应用卸载时释放。
- 日志仍限制为 500 条，进度事件不再自动膨胀为高频文本日志。

### 11.3 系统能力

目录选择使用 Tauri dialog plugin，打开父目录使用 opener plugin。Tauri capability 只开放主窗口所需的最小命令和插件权限，不启用通用 shell 执行。

## 12. 安全设计

- Registry 连接必须验证 TLS；禁止等价于 `verify=False` 的全局开关。
- 认证信息只在 Token 请求和受保护 Registry 请求中使用，不进入错误 details、日志或历史。
- 输出路径使用 Rust `PathBuf`，不通过 shell 拼接命令。
- 打开目录前规范化路径，并只调用系统 opener，不执行路径内容。
- tar 内路径由应用生成并验证为相对路径，避免绝对路径和 `..`。
- 配置写入使用用户私有应用数据目录；凭据使用 Windows Credential Manager。
- Tauri CSP 只允许应用自身资源和必要连接；Registry 网络请求由 Rust 发出，不放宽 WebView `connect-src`。

## 13. 测试策略

### 13.1 Rust 单元测试

- 镜像引用：短名称、命名空间、端口、Tag、Digest、非法输入。
- WWW-Authenticate 解析：Bearer 参数顺序、缺失字段、Basic credentials。
- 平台选择：amd64、arm64、arm/v7、缺失平台。
- 状态机：合法转换、双启动、取消、失败兜底、迟到事件。
- 设置 patch、配置迁移、原子写入和并发更新。
- Digest 校验、Layer ID、manifest.json 和 repositories 内容。

### 13.2 Rust 集成测试

使用本地 mock Registry，不依赖公网：

- 匿名和认证 Token 流程。
- Docker Manifest List 和 OCI Index。
- 并发 Layer 下载和进度聚合。
- 连接中断后 Range 续传。
- 服务端忽略 Range、返回 416、Manifest 变化。
- Digest 不匹配、TLS 无效、超时、取消。
- gzip 与未压缩 Layer。
- 最终 tar 条目和内容验证。

### 13.3 前端测试

- command 封装负载和错误转换。
- Pinia 对 state/progress/log 事件的处理。
- taskId 过滤和初始化快照收敛。
- 下载、取消、重新下载、设置 patch、历史操作。

### 13.4 端到端与构建验证

- Tauri 开发模式启动并完成 mock Registry 下载。
- Windows Release build 成功。
- 在未安装 Python、Node 和 Docker 的干净 Windows 环境启动便携 EXE。
- 目录选择、下载、取消、恢复、历史和打开目录可用。
- 若验证机有 Docker，执行 `docker load -i <tar>` 作为额外兼容性验收；自动测试仍以 tar 内容 fixture 为硬门槛。
- 记录最终 EXE 和解压运行依赖体积，与当前 Electron 分发和约 23 MB Python sidecar 对比。体积目标作为测量结果报告，不在首次设计中承诺未经构建验证的固定数字。

## 14. Windows 便携构建

Tauri 配置目标为 Windows x64。正式构建使用 release profile：

- `panic = "unwind"`，这是任务边界捕获 panic 并收敛到 `failed` 的必要条件；不得使用 abort 策略
- `lto = true`
- `codegen-units = 1`
- `strip = true`
- `opt-level = "s"`，若性能测试显示哈希或解压显著退化再调整

构建流程产出 Tauri 主程序，并将 release executable 复制到明确的 `release/portable/DockerPull.exe`。`bundle.active` 关闭或不声明 NSIS/MSI targets。便携 EXE 依赖 Windows WebView2；Windows 10/11 通常已具备，若目标环境缺失，应显示明确运行时安装提示，而不是捆绑固定 WebView2 运行时导致体积重新膨胀。

## 15. 分阶段迁移

### 阶段 1：契约与 Tauri 骨架

- 建立 `src-tauri`、Tauri 2 配置和 Windows capability。
- 定义领域模型、CommandError、commands 和 events。
- 实现 storage、配置迁移、CredentialStore。
- 前端增加 Tauri API 适配层，但暂不删除旧代码。

验收：Rust 单元测试通过，Tauri 空壳能加载现有 Vue 页面，设置和历史走新 commands。

### 阶段 2：Rust 下载核心

- 实现解析、认证、Manifest/平台选择和 Blob 客户端。
- 实现并发、续传、Digest、取消、进度、归档和全终态历史。
- 建立 mock Registry fixtures 和集成测试。

验收：所有下载核心测试通过，mock 镜像可生成结构正确的 tar。

### 阶段 3：前端切换

- 下载 store 切换到 commands/events。
- 设置、镜像源、历史、目录操作全部切换到 Tauri。
- 修正响应性和高频日志问题。
- 执行 Vue 类型检查和前端测试。

验收：应用不需要 FastAPI 即可完成所有 UI 流程。

### 阶段 4：删除旧架构并发布

- 删除 Electron 主进程、preload、PythonProcessManager 和相关依赖。
- 删除 `backend/`、`backend.spec`、`startup.config.json` 和 PyInstaller 构建链。
- 更新 README、开发命令和发布说明。
- 构建并验证 Windows x64 便携 EXE，记录最终体积。

旧代码只在 Rust 核心和前端切换验收后删除，避免迁移中途失去行为参照。最终主分支不得保留双运行时发布路径。

## 16. 验收标准

重构完成必须同时满足：

1. Windows x64 便携 EXE 在无 Python、Node、Docker 环境可启动。
2. 应用不监听 8000 或其他本机服务端口。
3. 短名称、命名空间、私有 Registry 和多架构镜像可正确处理。
4. 下载中断后可复用有效部分，服务端不支持 Range 时安全回退。
5. 每个 Blob 和 config 均进行 Digest 校验，损坏内容不能进入最终 tar。
6. TLS 证书错误不会被忽略。
7. 取消能收敛到 cancelled，应用可立即开始下一任务。
8. 命令查询和事件观察到的任务状态一致。
9. 成功、失败和取消均可在历史页查看。
10. 现有合法配置和历史首次启动后保留；写入不会因并发操作丢失更新。
11. Rust 测试、前端类型检查与测试、Tauri release build 全部通过。
12. 最终 tar 通过 fixture 结构验证，并在可用环境通过 `docker load` 验证。
13. 镜像源仍可在界面中新增、编辑、删除和设为默认；每个镜像源可保存独立认证信息，历史重新下载在原镜像源已删除时明确回退到当前默认源。

## 17. 后续可演进方向

目标架构刻意保留以下扩展点，但不进入本次首版：

- 多任务队列和任务持久化。
- OCI layout 直接输出。
- 用户导入企业 CA。
- 跨平台 macOS/Linux 桌面包。
- 镜像内容缓存、Layer 跨镜像去重和配额管理。
- 自动更新和代码签名。

这些能力应建立在 `registry`、`task_manager`、`archive` 和 `storage` 的稳定边界上，不重新把本机 HTTP 服务引入桌面应用内部。
