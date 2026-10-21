# agl — AgentLink

[English](README.md) | 简体中文

一个 Rust 单文件二进制（约 4MB），用来驱动本机的编码 Agent：Claude Code、Codex、Gemini、Kimi、OpenCode、Grok、Cursor 等。它提供：

- 常驻热会话、并发与排队、审批、运行中插话（steer）、中断、模型列表、自动垃圾回收；
- 一条 `curl … | sh` 就能把任意机器接入你的 Web 应用，网页和机器上的 Agent 双向通信。

设计参考了 [botiverse/oar](https://github.com/botiverse/oar)、douchat 和 Fleet 的本地运行时。各种接入方式的对比和实测数据见 [docs/comparison.md](docs/comparison.md)。

```
 浏览器 ──> 你的后端 / agl hub <──wss（机器主动外连）── agl daemon ─┬─ claude   （stream-json + 控制协议）
 终端 / 桌面 App / 运行时 ──ws://127.0.0.1──────────────────────────┤─ codex    （app-server JSON-RPC）
 Rust App ──进程内调用 agl::session::Manager────────────────────────┤─ gemini · kimi · opencode · grok · …（ACP）
                                                                   └─ cursor · pi（每轮启动一次，原生 resume）
```

## 能力

| | |
|---|---|
| **一套协议驱动 11+ 种 Agent** | 每个 Agent 都用它最好的第一方接口；还可以通过配置接入任意支持 ACP 的 CLI |
| **热会话、复用上下文** | 两轮之间进程不退出；被回收或重启后用原生 resume 接上原对话。实测 claude 每轮首字从约 6s 降到约 1s |
| **复用会话** | 用名字复用（`-s name`）。CLI、桌面 App、网页共用同一批会话，可以互相接着聊；会话历史落盘，带续传游标 |
| **并发、排队、steer** | 不同会话并行；同一会话内排队执行，或把消息插入正在进行的一轮（claude、codex） |
| **Computer use 与 browser use** | Codex 的 computer-use、browser-use 插件经 agl 照常可用：截图通过 `tool_end` 的 `images` 返回，按应用的操作确认以 `permission_request` 下发（`allow_session` 在本会话内记住该应用）。 |
| **审批** | 四档权限 `full` / `edit` / `ask` / `read_only`。claude、codex、ACP 的权限请求统一转成 `permission_request`，没人回答时超时自动拒绝 |
| **自动垃圾回收** | 进程数硬上限（按 LRU 驱逐空闲会话）、空闲挂起、过期删除；崩溃后自动清理残留进程 |
| **稳定性** | 有心跳和卡死检测；中断 10s 后仍未结束就强制结束；API key 冲突时自动改用 CLI 的登录账号；从登录 shell 导入 PATH 和 key |
| **远程接入** | 一次性配对码、每台机器独立 token、可吊销；安装脚本只允许 HTTPS、校验 sha256，并注册 launchd / systemd 开机自启 |
| **代理与内网** | 机器主动外连；Hub 连接支持 `HTTPS_PROXY` / `NO_PROXY`（launchd / systemd 启动时同样生效） |
| **一台机器连多个 web app** | 可以同时连多个 hub，每个 hub 只能看到自己创建的会话，以及本地会话（`--private` 时看不到）；远程创建的会话默认 `ask` |
| **接入方式** | 本地 WebSocket，零依赖 TypeScript 客户端，Rust crate，给编码 Agent 用的接入 skill |

## 安装

```bash
# 预编译二进制（macOS / Linux），安装到 ~/.agl/bin，并校验 sha256
curl -fsSL https://github.com/thinkany-ai/agl/releases/latest/download/install.sh | sh

# 从源码安装（需要较新的 stable Rust）
cargo install --git https://github.com/thinkany-ai/agl

# 或者克隆后构建
git clone https://github.com/thinkany-ai/agl.git && cd agl
cargo build --release          # 产物：target/release/agl
```

支持 macOS（Apple 芯片 / Intel）和 Linux（x64 / arm64）；Windows 暂不支持。

## 命令行

```bash
agl agents                                   # 检测已安装的 agent 和各自支持的能力
agl models codex                             # 列出可用模型（不会创建对话）
agl run claude "这个仓库是做什么的？"          # 一次对话（daemon 会自动在后台启动）
agl run claude -s proj --access ask "修复失败的测试"   # 命名会话；ask 模式会在终端里询问是否允许
agl send proj "再跑一遍测试"                  # 继续已有会话
agl send proj --steer "顺便把 lint 也修了"     # 插入正在进行的一轮
agl chat kimi -s k                           # 交互式聊天（ACP Agent）
agl configure proj -m opus --access edit     # 改模型或权限，保留对话
agl approve proj <request_id> allow          # 在另一个终端处理审批
agl sessions | history proj | attach         # 列表 / 历史 / 实时跟踪所有会话
agl abort proj | close proj | gc --idle
agl status | start | stop
```

## 接入你的产品

**给编码 Agent 用的 skill**：[skills/agl-integration](skills/agl-integration/SKILL.md)。把这个目录复制到 `~/.claude/skills/`（或你的 Agent 的 skills 目录）后，说"用 agl 接入本地 agent"即可。下表是各种产品形态的接法：

| 产品形态 | 做法 | 文档 |
|---|---|---|
| 桌面 App（Electron / 原生），关掉 App 就不用 | 作为 sidecar 打包：`agl daemon --port 0 --parent-pid <pid>`，使用独立的 `AGL_HOME` | [desktop-app.md](skills/agl-integration/references/desktop-app.md) |
| Rust 桌面 App（Tauri / GPUI） | 直接链接 `agl` crate，在进程内使用 `Manager` | [desktop-app.md#rust-in-process](skills/agl-integration/references/desktop-app.md#rust-in-process) |
| Web 产品 | 用户执行 `curl …/install.sh?code=… \| sh`，机器主动外连到你的后端 | [web-product.md](skills/agl-integration/references/web-product.md) |
| 已有自研运行时 / daemon | 保留你的配对和传输层，只把执行层换成 agl | [existing-runtime.md](skills/agl-integration/references/existing-runtime.md) |

协议参考：[protocol.md](skills/agl-integration/references/protocol.md)。客户端：[clients/typescript/agl.ts](clients/typescript/agl.ts)。

### 参考 Hub

```bash
agl hub --listen 0.0.0.0:8787 --public-url https://hub.example.com
```

打开 `https://hub.example.com/?token=<admin>`，点"连接电脑"即可。Hub 会为每次连接生成一条带一次性配对码的安装命令，也可以复制安装说明交给 Agent 去执行。

Hub 网页支持：
- 新建会话时选 Agent、模型和权限；
- 流式对话；
- 在网页上审批；
- 运行中插话、停止；
- 显示首字时间和总耗时；
- 吊销机器。

Hub 的二进制分发：Hub 从 `--dist <dir>` 目录提供 `agl-<os>-<arch>` 及其 `.sha256`，和 Hub 同平台的那份可以直接提供；其他平台会重定向到 GitHub 最新 release（`--release-url`）。

## 配置

配置文件在 `~/.agl/config.json`，完整字段见 [protocol.md](skills/agl-integration/references/protocol.md#configuration-aglhomeconfigjson)。

```json
{ "default_access": "full", "max_live": 8, "idle_timeout_secs": 600, "approval_timeout_secs": 600,
  "custom_agents": [{ "id": "goose", "command": "goose", "args": ["acp"], "protocol": "acp" }] }
```

`~/.agl/` 下的其他文件：

| 文件 | 内容 |
|---|---|
| `daemon.json` | 端口和本地 token，权限 0600 |
| `sessions.json` | 会话列表 |
| `history/` | 每个会话的事件历史 |
| `daemon.log` | daemon 日志 |

## 仓库结构

| 路径 | 内容 |
|---|---|
| `src/agents/` | 各 Agent 驱动：`claude.rs`、`codex.rs`、`acp.rs`、`oneshot.rs` |
| `src/session.rs` | 会话管理：热会话、排队、steer、审批、GC、进程预算 |
| `src/daemon.rs`、`src/link.rs` | 本地 daemon 与连接 Hub 的机器链路 |
| `src/hub.rs`、`assets/` | 参考 Hub、网页 UI、安装脚本 |
| `clients/typescript/` | 零依赖 TypeScript 客户端 |
| `skills/agl-integration/` | 给编码 Agent 用的接入 skill |
| `docs/` | 方案对比与实测 |

## 安全

- `full` 等于让 Agent 以你的身份执行任意命令。Hub 创建的会话默认 `ask`（可用 `agl connect … --access` 按连接修改），远程使用时 Agent 执行有风险的操作前都会先询问。
- 本地 daemon 只监听 127.0.0.1，每个连接都要带 `daemon.json` 里的 token。
- Hub 的机器 token 服务端只存哈希，可以单独吊销；配对码 10 分钟有效，只能用一次。

## 许可证

[MIT](LICENSE) © 2026 ThinkAny, LLC
