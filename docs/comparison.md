# 对接本地 Agent 的几种方式：ACP、codex app-server、claude stream-json、一次性调用、oar 与 agl

本文回答两个问题：

1. 每个 Agent 应该用哪种接口去驱动？
2. 产品为什么应该接入 agl，而不是自己去对接这些接口？

结论和依据来自三个来源：
- 对 oar（botiverse/oar）文档与实验记录的梳理；
- douchat、Fleet 两个已有本地运行时的实现；
- 本仓库在本机实测的协议探测和基准测试（2026-10-21，macOS arm64）。

## 1. 四种 Agent 接口

| | ACP（Agent Client Protocol） | codex `app-server` | claude `-p` stream-json | 每轮启动一次 CLI + `--resume` |
|---|---|---|---|---|
| 传输 | stdio JSON-RPC 2.0（NDJSON），双向 | stdio JSON-RPC，双向 | stdio NDJSON，消息帧和 `control_request` / `control_response` 走同一通道 | 参数传入，NDJSON 输出，进程结束即本轮结束 |
| 谁支持 | Gemini、Kimi、OpenCode、Grok、Droid、Qwen、Goose、Copilot、OpenClaw、Hermes、OMP 等，约 40 个 | 仅 Codex | 仅 Claude Code（Agent SDK 底层用的也是它） | 所有 CLI |
| 常驻多轮（热会话） | ✓ | ✓ | ✓ | ✗，每轮都冷启动，包括 MCP 连接和 hooks |
| 流式粒度 | `agent_message_chunk`、`agent_thought_chunk`、`tool_call(_update)` | `item/agentMessage/delta`、命令输出 delta、item 生命周期 | token 级 delta（`--include-partial-messages`），工具入参也是流式的 | 只能事后解析输出 |
| 运行中插话（steer） | 没有标准方式，各家自定（Grok 是 `sendNow` 打断重跑，OpenCode v1 能接第二个 prompt） | `turn/steer {expectedTurnId}`，有竞态保护 | 运行中直接写入新的 user 帧，在下一个模型步骤被吸收 | ✗ |
| 中断 | `session/cancel` 通知，prompt 以 `stopReason: cancelled` 返回 | `turn/interrupt`，之后 `turn/completed status: interrupted` | `control_request {subtype: interrupt}` | 只能杀进程 |
| 权限审批往返 | `session/request_permission`（allow_once / allow_always / reject…） | 服务端请求：`commandExecution` / `fileChange` / `permissions` / elicitation | `--permission-prompt-tool stdio` 时发 `can_use_tool` | ✗，只能用启动参数整体放开或整体拒绝 |
| 模型列表与切换 | session 的 `configOptions`（`category: model`）和 `session/set_config_option`；`set_model` 已于 2026-06 移除 | `model/list`、`thread/settings/update` | 控制协议 `initialize` 返回 `models`，另有 `set_model` | 只能用启动参数 |
| 恢复会话 | `session/resume`（不重放）、`session/load`（会重放历史） | `thread/resume`（每次要重新传 model，否则回到默认） | `--resume <id>` | `--resume <id>` |
| 已知坑 | 厂商实现常与自己声明的能力不一致；没有 turn id；子 agent 的可见性各家不同 | 同一个 `CODEX_HOME` 首次并发 `initialize` 会触发 SQLite 竞态（实测 21/24 失败，openai/codex#50290）；中断会丢弃尚未被模型吸收的 steer | Bash 工具进程脱离进程组；SessionStart hooks 会阻塞初始化；有 `ANTHROPIC_API_KEY` 时优先用 key 而不是登录账号 | 冷启动慢；中断时会留下"工具被中断"的记录 |

**选型结论**（与 oar 的结论一致）：
- **一律使用各家的第一方接口**：claude 用 stream-json，codex 用 app-server，其他 Agent 用它们自带的 ACP。
- **不要**用第三方的 ACP 适配器（`claude-agent-acp`、`codex-acp`）去接 Claude 或 Codex。适配器会合并文本、改写工具结果、按自己的策略回答审批，相当于在你和 Agent 之间多了一个"事实的第二所有者"。
- 一次性调用只留给没有常驻协议的 CLI（cursor-agent、pi），以及崩溃后恢复的场景。

## 2. 实测：冷启动 vs agl 热会话

**测试方法**
- 同一会话连续 4 轮，prompt 都是 "Reply with exactly: …"。
- claude 用 `haiku`；codex 用默认模型（gpt-6.1-sol）。
- 冷启动组模拟 dscode、Fleet 现在的做法：每轮新开一个 `claude -p … --resume <id>`，或 `codex exec resume <id>`。
- 热会话组是 agl 的常驻进程。
- 计时从客户端发出请求开始。脚本在 `bench/cold-vs-warm.mjs`，原始数据见下。

| | 首字（中位数） | 本轮总耗时（中位数） |
|---|---|---|
| claude 冷启动（每轮 `claude -p --resume`） | 6.2s（4.1–6.7） | 7.8s（6.4–8.6） |
| **claude agl 热会话，第 2 轮起** | **1.3s**（0.9–2.4） | **1.3s** |
| claude agl 第 1 轮（已预热） | 3.6s | 3.7s |
| codex 冷启动（每轮 `codex exec resume`） | 5.1s（4.4–6.5） | 8.3s（7.5–9.8） |
| **codex agl 热会话** | **2.7s**（2.0–3.7） | **2.9s**（2.1–3.8） |

- **claude**：热会话把每轮首字时间从约 6s 降到约 1s，快 4–5 倍。
- **codex**：总耗时快约 2.8 倍。codex 的回复通常在一条消息里一次性完成，所以它的首字时间和总耗时很接近。
- **API key 冲突**：本机登录 shell 里有一个余额不足的 `ANTHROPIC_API_KEY`。agl 第一次遇到时会自动改用 claude 的登录账号重试，代价是约 5s，之后整台机器都会记住（`state.json`），新会话不会再付这个代价。冷启动方案每次都要么失败、要么手动处理。

原始数据：

```
claude cold      first_text_ms [6719, 6560, 5869, 4092]  total_ms [8552, 7138, 8417, 6409]
codex  cold      first_text_ms [6534, 4795, 5501, 4424]  total_ms [9789, 7781, 8757, 7522]
claude agl warm  first_text_ms [3639, 1285, 900, 2371] / [10135*, 1270, 947, 1829]
codex  agl warm  first_text_ms [3731, 2823, 2993, 2536] / [2939, 2104, 2008, 2042]
* 首次检测到 key 冲突的那一轮，包含一次失败加重试
```

## 3. 产品层面的几种做法

| | 自己写一次性调用（dscode） | 自研常驻运行时（douchat） | 自研 daemon + 一次性调用（Fleet） | oar（TS 库） | **agl** |
|---|---|---|---|---|---|
| 形态 | 嵌在应用里 | 嵌在 Electron 主进程里 | Node 守护进程，长轮询 | Node 库，进程内调用 | 单个 Rust 二进制：daemon、库、CLI、Hub |
| 热会话 | ✗ | claude / codex | ✗ | ✓ | ✓，claude / codex / 所有 ACP Agent |
| 覆盖的 Agent | 5 种，一次性调用 | 2 种常驻 + 若干一次性 | 12 种，一次性调用或 ACP | 8 种 | 11 种内置 + 自定义 ACP |
| 审批往返 | ✗ | ✓ | ✗，全部拒绝或绕过 | ✗，刻意不做 | ✓，claude / codex / ACP 统一转成 `permission_request` |
| 运行中插话 | ✗ | ✗ | ✗ | ✓ | ✓（claude、codex） |
| 进程上限 / 空闲回收 / LRU | ✗ | ✓ | 并发 4 | 宿主自己管 | ✓，有硬上限，满了驱逐空闲会话 |
| 历史持久化与续传游标 | 自己做 | 自己做 | 服务端 | 宿主自己管 | ✓，`seq` 加 `session.history` |
| 远程接入（curl 安装 + 配对） | ✗ | douchat-host | ✓ | ✗ | ✓，配对码换机器 token，可吊销 |
| 接入成本 | 每个 CLI 都要自己写解析 | 只有内部能用 | 只有内部能用 | 只能 Node | 任何语言，只要会用 WebSocket；Rust 可以直接链接 |

## 4. agl 为什么更快、更稳、接入更方便

**更快**
- 常驻进程，只有第一轮付启动成本。
- `warm: true` 和 `session.warm` 可以在用户打字时就把进程拉起来。claude 会立即发送 `initialize`，让 MCP 和 hooks 提前完成，而不是等第一条消息才开始。
- 改模型、改权限时用原生 resume 换进程，不丢上下文，也不用重新灌历史。
- `turn_ended.timing` 给出每轮的 `launch_ms`、`first_event_ms`、`first_text_ms`、`total_ms` 和 `warm`，方便产品自己做监控。

**更稳**

| 机制 | 怎么做的 | 参考来源 |
|---|---|---|
| 进程清理 | 每个 Agent 一个独立进程组；结束时先收集整棵子孙进程（工具进程会脱离进程组），SIGTERM 后 3s 再 SIGKILL | – |
| 崩溃恢复 | 新 daemon 启动时，按"启动时间 + 命令行"指纹清理旧 daemon 留下的进程，不会误杀被复用的 pid | Fleet |
| 不会卡死 | 不认识的服务端请求一律回错误，不让 CLI 干等；审批超时自动拒绝；中断后 10s 没有结束就强制结束 | – |
| 卡死检测 | 只有在 CLI 报告过重试之后仍然 120s 没有输出，才判定为卡死，不会误杀长时间运行的工具 | douchat |
| 失败重试 | 不盲目重试失败的轮次，只在本轮还没有任何输出时重试：API key 冲突时改用登录账号；resume id 丢失时开新会话，并明确告诉用户 | douchat |
| codex | 握手串行化；resume 遇到 `active writer` 时退避重试；resume 返回的 thread id 不一致就直接拒绝 | oar、Fleet |
| 输入帧 | 单帧最大 64MB，超大帧整帧丢弃，不会把内存撑爆 | douchat |
| 环境 | 从登录 shell 读取 PATH 和 API key（`-lic` 加输出标记），额外搜索约 25 个常见安装目录和 nvm；会检查 shebang 解释器是否存在，避免执行坏掉的脚本 | douchat |
| 远程接入 | 配对码只能用一次；每台机器独立 token，服务端只存哈希；机器被吊销后停止重试；安装脚本只允许 HTTPS，并校验 sha256 | douchat、Fleet |

**接入更方便**
- 一套协议覆盖所有 Agent：本地 WebSocket、Hub 机器链路、Hub 浏览器端完全一致。
- 零依赖的 TypeScript 客户端：`clients/typescript/agl.ts`。
- Rust 应用可以直接用 `agl` crate，在进程内运行。
- sidecar 模式只要三个参数：`--port 0`、`--parent-pid`、独立的 `AGL_HOME`。
- 给编码 Agent 用的接入 skill：`skills/agl-integration`，按产品类型给出步骤和验收清单。

## 5. 仍然不如别人的地方（待做）

- **Windows**：进程组和服务都还没做。douchat 有 `taskkill` 和 npm shim 的处理可以参考。
- **ACP 的 steer**：Grok 的 `sendNow`、OpenCode v1 可以接收第二个 prompt，这些厂商扩展还没有接，现在一律排队。
- **ACP 客户端能力**：还没实现 `terminal/*` 和 `fs/*`。目前声明为不支持，由 Agent 自己执行命令。
- **多 session 共享进程**：codex app-server 本身支持一个进程跑多个 thread，但 agl 仍然是一个会话一个进程（oar 也没有验证过前者）。
- **输入身份**：可以用 claude 的 `uuid` 加 `--replay-user-messages`、codex 的 `clientUserMessageId` 做到逐条输入的确认。目前只做了 `turn_id` 级别。
- **长轮询链路**：Hub 链路只有 WebSocket。多 pod 的后端需要一个有状态的网关（见 skill 的 web-product.md），Fleet 那种 HTTP 长轮询方式还没提供。
- **Agent 登录**：agl 不负责 CLI 的登录流程（oar 有 `login()`），用户需要先在机器上登录好各个 CLI。无头服务器上可以靠 API key 环境变量解决。

0.2 已补上的：
- 预编译二进制（GitHub Releases，Linux 为静态 musl 版本）；
- 一台机器连多个 hub，并按来源隔离会话；
- 远程会话默认 `ask`；
- 协议版本号；
- Linux 下用 `/proc` 查进程树，不再依赖 `ps`；
- 用模拟 agent 做的端到端测试，以及在 CI 里跑测试。
