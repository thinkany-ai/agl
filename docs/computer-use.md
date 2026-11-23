# 设计：内置 computer-use 与 browser-use（草案）

状态：第一期已实现（agl 0.3.16；截图直传、去重与 `app_name` 参数见 0.3.17，第 4.1 节），接入说明见
[local-tools.md](../skills/agl-integration/references/local-tools.md#computer-use-macos-agl-0316)
和 [protocol.md](../skills/agl-integration/references/protocol.md)。

### 实现与本设计的出入

| 设计 | 实际 |
|---|---|
| ScreenCaptureKit 截图 | 用系统的 `screencapture -l<窗口id>` 截窗口、`sips` 转 JPEG，不引入 ObjC 依赖；授权同样归属产品 launcher |
| AX 通知判断界面稳定 | 固定等待 350ms 后读取新状态 |
| `tools.list` 返回 `instructions` | 不改变 `tools.list` 的数组格式，使用说明写进 `computer_list_apps` 的描述 |
| 坐标点击走 `CGEventPostToPid` | 先用辅助功能命中测试（`AXUIElementCopyElementAtPosition`）找到该点的控件，按下或选中它；找不到才发鼠标事件 |
| 快捷键直接发给进程 | 后台时先在 App 菜单栏里找快捷键相同、且可用的菜单项并按下；`cmd+a` 在文本控件上直接设置选区；都不行再发键盘事件 |
| 浮层 + 全局停止快捷键 | **未做**。停止靠 hub 调用 `computer.stop` / `computer.reset`（页面上的停止按钮） |
| 每个 App 可以并行 | 每个产品一个辅助进程，请求串行 |
| 默认禁止调用方产品自己的 App | 只禁止 agl 生成的 launcher（`cc.agl.app.*`）；产品的桌面 App 可通过 `computer.blocked_apps` 加入 |

本机实测（macOS 26）：Surge、TextEdit 上读树、截图、按元素和按坐标点击、插入文本、键盘输入、`cmd+a`、设值、过期检测都在后台完成，前台 App 始终没有变化。

已知限制：后台时，依赖菜单验证的快捷键（如 TextEdit 的“全选”）对应的菜单项是灰的，按下没有效果，所以会退回发键盘事件，而带 cmd 的键盘事件在后台通常也不生效。这种情况结果里会提示模型改用 `foreground: true`。

## 1. 目标

用户电脑上只装了 agl，没有 Codex、Claude Code 等本地 Agent。云端产品用自己的模型和
agent loop，通过 agl 操作这台电脑上的 App 和浏览器，体验与 Codex 的 Computer Use /
Browser Use 一致：

| Codex 的体验 | agl 的做法 |
|---|---|
| 以 App 为单位操作，先读界面结构再看截图 | `computer_get_state` 返回辅助功能树（元素带编号）+ 窗口截图 |
| 后台操作，不抢鼠标和焦点 | 优先用 AX 动作；键盘事件发给目标进程；只截单个窗口 |
| 每个 App 首次使用前确认，本会话内记住 | 按 App 审批，`allow_session` 以 (session, App) 为单位 |
| 截图在对话里实时可见 | `tools.call` 结果带 `images` |
| 随时可以停止 | 屏幕浮层 + 全局停止快捷键，以及 hub 侧中止 |
| 系统权限弹窗显示产品名 | 辅助进程跑在 `app_name` launcher 下（复用 `src/app.rs`） |

不做的事：

- 不复用 Codex 的插件（`cua_repl`、`node_repl`、Chrome 扩展）。它们是闭源组件，随 Codex.app 分发，没有 Codex 就不存在。
- 不引入 Node、Python 运行时，agl 仍然是单个二进制。
- 第一期不做 Linux 和 Windows。

## 2. 分期

| 期 | 内容 |
|---|---|
| 一 | macOS `computer_*` 工具；`tools.call` 结果支持图片；按 App 审批；权限检查方法 |
| 二 | `browser_*` 工具：用 CDP 控制专用 profile 的 Chrome、Edge 或 Chromium |
| 三 | Chrome 扩展 + native messaging，用于用户自己已登录的浏览器；`agl mcp` 把内置工具暴露给本地 Agent；Linux（AT-SPI） |

以下是第一期的详细设计，第二期只列出大纲（第 9 节）。

## 3. 工具定义（`tools.list`）

在 `read / write / edit / exec` 之外新增以下工具。只有满足下面三个条件时，`tools.list` 才会列出它们：

- 运行在 macOS 上；
- 辅助进程能启动；
- 当前链路的权限允许（见第 6 节）。

machine 的 `tools` 字段同样只在满足这些条件时带上这些名字。

所有工具都以 `app` 指定目标 App，取值是 `computer_list_apps` 返回的 `bundle_id`，也接受 App 的显示名，但显示名有歧义时报错。

| 工具 | 参数 | 作用 |
|---|---|---|
| `computer_list_apps` | – | 列出正在运行且有窗口的 App：`[{bundle_id, name, pid, windows: [{id, title, focused}], blocked}]` |
| `computer_get_state` | `app, window?, screenshot? = true, max_elements? = 1500` | 返回 `state_id`、辅助功能树文本，以及窗口截图（放在 `images` 里） |
| `computer_click` | `app, element?, x?, y?, button? = left, count? = 1, state_id?` | `element` 和 (`x`,`y`) 二选一 |
| `computer_type` | `app, text, element?, state_id?` | 输入文本；给了 `element` 时先聚焦该元素 |
| `computer_key` | `app, keys` | 发送组合键，如 `"cmd+l"`、`"return"`、`"shift+tab"` |
| `computer_scroll` | `app, element? \| x?,y?, direction, amount? = 3, state_id?` | 滚动 |
| `computer_drag` | `app, from: {element}\|{x,y}, to: {element}\|{x,y}, state_id?` | 拖拽 |
| `computer_set_value` | `app, element, value, state_id?` | 直接给控件设值（AXValue），适合文本框、滑块 |
| `computer_action` | `app, element, action, state_id?` | 执行元素支持的其他 AX 动作，如 `AXShowMenu`、`AXIncrement`、`AXConfirm` |
| `computer_open_app` | `app` | 启动或激活 App（`NSWorkspace`），这是唯一会把 App 切到前台的工具 |

每个动作工具执行完都返回操作后的新状态，与 `computer_get_state` 的结果格式相同。这样模型少一轮往返，Codex 也是这样做的。可以传 `return_state: false` 关掉。

### 3.1 界面树格式

返回紧凑文本，而不是 JSON，以节省 token：

```
window "Untitled — Notes" [1]
  toolbar [2]
    button "New Note" [3]
    textfield "Search" value="" [4]
  splitgroup [5]
    table "Notes" [6]
      row "Groceries" selected [7]
    textarea value="milk, eggs…" focused [8]
```

- `[n]` 是元素编号，只在对应的 `state_id` 内有效。
- 只保留可交互的元素和有文字的元素，不可见和零尺寸的元素会被剪掉。
- 文本值超过 200 字符时截断，标注 `…`。
- 元素数超过 `max_elements` 时，`truncated: true`。
- 每个元素的 frame 不放进文本，由 agl 内部保留，供坐标兜底和截图标注使用。

### 3.2 `state_id` 与过期检测

`computer_get_state` 生成一个 `state_id`，agl 为每个 (session, App) 保留最近一份元素表。

动作工具带 `element` 时，可以带上 `state_id`：

- 如果 `state_id` 不是最新的，或者对应元素已经失效，返回错误 `stale_state`，并附上新状态，让模型重新决定。不会去点错的东西。
- 不带 `state_id` 时，使用该 App 最近一份状态。

### 3.3 坐标与截图

- 截图用 ScreenCaptureKit 只截目标窗口，被遮挡、在后台的窗口也能截到。
- 长边缩放到不超过 1280px（0.3.17 起；0.3.16 为 1568px），格式为 JPEG，质量 70（0.3.16 为 80）。
- 坐标一律使用**截图像素坐标**，原点在窗口左上角，由 agl 换算成屏幕坐标。结果中带 `screenshot: {width, height, scale}`。

## 4. 结果格式：图片

`tools.call` 的结果新增可选的顶层字段 `images`，格式与 `tool_end.images` 一致：

```json
{
  "state_id": "s_9f2c",
  "app": {"bundle_id": "com.apple.Notes", "name": "Notes", "pid": 812},
  "window": {"id": 4411, "title": "Untitled — Notes"},
  "tree": "window \"Untitled — Notes\" [1]\n  …",
  "truncated": false,
  "screenshot": {"width": 1280, "height": 860, "scale": 0.5},
  "images": [{"mime_type": "image/jpeg", "data": "<base64>"}]
}
```

- 图片超过 4 MB 时，带 `omitted: true`，不带 `data`，与现有规则一致。
- 不认识 `images` 字段的旧 hub 会照常把整个结果序列化给模型，这就是 base64 文本，浪费 token。所以只有调用方声明 `images: true` 时才返回图片（`tools.call {…, images: true}`）；否则 `images` 里只有 `omitted: true`。
- TypeScript 客户端新增辅助函数 `toModelContent(result)`：拆出 `images`，返回 `[{type: "text", text: JSON.stringify(rest)}, ...图片块]`，分别给出 Anthropic 和 OpenAI 两种格式的写法。没有 `data` 的图片（`omitted`、`uploaded`、`unchanged`）不放进去。

### 4.1 截图直传、去重与产品名（0.3.17）

链路有请求体上限（`max_body`）时，一张截图要切成几十个分片顺序发送，每一步都慢。0.3.17 在 `tools.call` 上新增三项，旧调用方不受影响：

- **`image_upload: {url}`**（需同时 `images: true`）：daemon 把截图的 JPEG 字节 `PUT` 到这个签名地址（`Content-Type: image/jpeg`，30 秒超时，不跟随重定向；只接受 HTTPS，或指向本机的 HTTP），结果中的图片变成 `{mime_type, uploaded: true, bytes}`，`bytes` 是 base64 长度，即原来 `data` 的长度。上传失败时照旧内联 `data`，并在 `warnings` 里加一条 `image_upload_failed: <原因>`。错误信息和日志都不包含这个地址，它是签名凭据。
- **不变的截图不重发**：按（链路、`session`、bundle id、窗口 id）记住最后发出的截图的哈希；新截图相同时，图片变成 `{mime_type, unchanged: true}`，既不内联也不上传。显式 `screenshot: true` 同样去重。`computer.reset {session}` 清空该会话的记录。
- **`app_name`**：链路本身没有 `app_name` 时（例如由网页配对流程建立的链路），辅助进程以调用方给的产品名运行，macOS 的辅助功能和屏幕录制授权因此归属该产品的 launcher；链路自己的 `app_name` 优先。`computer.permissions` 同样接受 `app_name`。等待审批的调用会保存这些选项，批准后照常生效。

## 5. 进程结构与系统权限

```
agl daemon ──stdio JSON──> $AGL_HOME/apps/<app_name>.app/…/agl __hold agl __computer
                                   (launcher, disclaims responsibility; TCC 归属到 <app_name>.app)
```

- **辅助进程**：每个 `app_name` 一个常驻辅助进程（`agl __computer`），由 launcher 启动，所以辅助功能和屏幕录制的授权都记在产品的 `.app` 上。没有 `app_name` 时归属 agl。空闲 10 分钟后退出。
- **为什么不在 daemon 里直接调用**：daemon 由 launchd 启动，系统授权会归到 agl 身上，多个产品就会共用一份授权，这正是 0.3.13 要解决的问题。辅助进程崩溃也不会拖垮 daemon。
- **辅助进程需要的权限**：
  - 辅助功能：`AXIsProcessTrustedWithOptions`；
  - 屏幕录制：`CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`。
- **新增方法 `computer.permissions {request?: bool}`**：返回 `{accessibility, screen_recording}`，取值为 `granted | denied | unknown`。`request: true` 时触发系统弹窗，并打开系统设置的对应面板。hub 可以调用它，产品据此在页面上做引导。
- **权限缺失时**：工具调用返回错误 `permission_missing: accessibility`（或 `screen_recording`），模型和产品都能看懂。
- **launcher 的签名身份**：launcher 是 ad-hoc 签名的 agl 拷贝，只在 `HOLDER_REV` 变化时重建，所以 agl 升级不会让授权失效。辅助进程的协议变更不应该触发 `HOLDER_REV` 递增。辅助进程由 launcher 以 `__hold` 方式拉起，执行的是 daemon 的 agl 路径，而不是 launcher 自身的拷贝，因此和 launcher 解耦。

### 5.1 后台操作的实现

| 动作 | 首选 | 兜底 |
|---|---|---|
| 点击元素 | `AXPress`（必要时用 `AXConfirm` 或 `AXPick`） | 用元素中心点坐标走坐标点击 |
| 坐标点击 | `CGEventPostToPid` 鼠标事件 | 部分 App 不接受发给进程的鼠标事件，返回错误 `needs_foreground`；模型可以先调 `computer_open_app`，再用 `foreground: true` 重试 |
| 输入文本 | 聚焦元素后用 `CGEventPostToPid` 发键盘事件（`CGEventKeyboardSetUnicodeString`） | 对文本控件用 `AXValue` 直接设值 |
| 组合键 | `CGEventPostToPid` | – |
| 滚动 | `AXScrollToVisible`，或在滚动区域上发滚轮事件 | – |

原则：除了 `computer_open_app` 和 `foreground: true`，不激活 App，不移动真实鼠标。

### 5.2 停止与可见性

- 辅助进程在操作期间，在目标窗口边缘显示一个不抢焦点的 `NSPanel` 浮层："<产品名> 正在使用 <App>"，上面有停止按钮。
- 全局快捷键（默认 `Esc` 连按两次，可配置）同样表示停止。
- 停止之后：正在执行的动作中止，这个 `session` 后续所有的 `computer_*` 调用都返回错误 `stopped_by_user`，直到 hub 调用 `computer.reset {session}`。hub 应该在用户发出下一条消息时调用它。

## 6. 权限与审批

`computer_*` 的能力相当于 `exec`，可以间接做任何事，所以默认比文件工具更严格。

**先要用户打开（0.3.17 起）**：每条 hub 链接默认关闭 computer use，`hello` 的 `tools` 和 `tools.list` 都不列出这些工具，调用返回 `computer use is off for this link`。只有用户在本机打开后，下表才生效：`agl computer-use on [hub]`，或者在 `agl connect` 时加 `--computer-use`（关闭用 `agl computer-use off [hub]`，配置里是该链接的 `computer_use`）。hub 无法替用户打开。嵌入 agl 的产品可以用 `Brand::computer_use_default` 让自己的链接默认打开（它自己的 App 带着 computer-use 辅助程序，用户安装它就是在选择这项能力）；本机客户端不受此限制。


| 链路 access | 不传 `approvals` | 传 `approvals: true` |
|---|---|---|
| 本地客户端 | 全部可用，不询问 | 同左 |
| `full` | 全部可用，不询问（与驱动 Codex 时 `full` 自动通过 App 确认一致） | 同左 |
| `edit` / `ask` | 不列出 | 列出；每个 App 第一次使用前需要确认 |
| `read_only` | 不列出 | 不列出（截图同样属于读取隐私，不在只读范围内） |

- **审批粒度是 App**：
  - 对某个 App 的第一次调用（包括 `computer_get_state`）返回 `approval_required`，`title` 为 `"使用 Notes（com.apple.Notes）"`，`tool` 为 `computer`。
  - `allow_session` 记住 (session, bundle_id)，时长与现有规则一致（一小时）。
  - `computer_list_apps` 不需要确认。
  - 需要扩展 `approvals.rs`：授权 key 从 `(origin, session, tool)` 改成 `(origin, session, scope)`，现有工具的 scope 就是工具名，computer 的 scope 是 `computer:<bundle_id>`。
- **默认禁止的 App**：无论什么 access 都不能操作，`computer_list_apps` 中标记为 `blocked: true`，调用返回错误 `app_blocked`。
  - 终端类：Terminal、iTerm2、Ghostty、Warp、WezTerm、Alacritty、kitty（否则等于绕过 `exec` 的审批）；
  - 钥匙串访问、1Password、Bitwarden 等密码管理器；
  - 系统设置（防止模型给自己授权）；
  - agl 生成的 `apps/*.app`，以及调用方产品自己的桌面 App（通过 `app_name` 对应的 bundle id 判断）。
- **名单配置**：放在 `config.json` 的 `computer.blocked_apps`（追加）和 `computer.allowed_blocked_apps`（解除默认禁止）。只能在本地修改，hub 不能改。
- **审批后执行前**：再检查一次链路 access 和禁止名单，与 0.3.15 的规则一致。

## 7. 并发与限制

- 同一个 App 的操作串行执行，不同 App 之间可以并行。
- 每次动作后等待界面稳定（AX 通知或最多 1500ms），再生成新状态。
- 单次动作的超时是 15s。
- 截图和界面树只放在内存里，不落盘，也不写进 history。

## 8. 给模型的使用说明

在 `tools.list` 结果中新增 `instructions`（字符串），只在列出 computer 工具时出现。云端把它拼进 system prompt。内容大意：

- 能用文件、命令或 API 完成的，不要用 computer；
- 先 `computer_get_state`，优先按 `element` 操作，坐标只作兜底；
- 每一步之后看返回的新状态，确认生效再继续；
- 遇到登录、支付、删除、发送消息之类不可逆的操作前，先向用户确认；
- 收到 `stopped_by_user` 立即停止，不要重试。

## 9. 第二期大纲：browser

- **启动方式**：agl 用 `--remote-debugging-pipe --user-data-dir=$AGL_HOME/browser/<app_name>` 启动本机的 Chrome、Edge、Brave 或 Chromium，用 Rust 写一个精简的 CDP 客户端，不需要 Node 或 Playwright。
- **为什么用专用 profile**：Chrome 136 起，默认 profile 下的远程调试端口不再生效。要用用户自己的浏览器，只能走第三期的扩展方案。
- **工具**：`browser_navigate`、`browser_snapshot`（基于 CDP `Accessibility.getFullAXTree`，元素带 ref，格式与第 3.1 节一致）、`browser_click`、`browser_type`、`browser_key`、`browser_scroll`、`browser_screenshot`、`browser_tabs`、`browser_evaluate`。
- **审批粒度**：按站点（origin）确认，结构与 computer 的按 App 确认相同。
- **找不到 Chromium 系浏览器时**：报错 `no_browser`，`instructions` 提示模型改用 computer 操作 Safari。是否支持按需下载 Chrome for Testing，第二期再定。

## 10. 实现落点

| 模块 | 内容 |
|---|---|
| `src/machine/tools.rs` | 注册 `computer_*` 的 schema；`tools.call` 透传 `images`；`instructions` |
| `src/machine/approvals.rs` | scope 化授权 key；computer 的 gate 规则；禁止名单检查 |
| `src/machine/computer.rs`（新增） | daemon 侧：辅助进程池（按 `app_name`）、请求转发、`state_id` 缓存、停止标志 |
| `src/computer/`（新增，仅 macOS） | 辅助进程：`ax.rs`（树、动作）、`capture.rs`（ScreenCaptureKit）、`input.rs`（CGEvent）、`overlay.rs`（浮层与快捷键） |
| `src/main.rs` | `__computer` 子命令入口 |
| `clients/typescript/agl.ts` | `images` 类型、`toModelContent`、`computerPermissions`、`computerReset` |

依赖倾向使用 `objc2` 系列（`objc2-app-kit`、`objc2-screen-capture-kit`、`objc2-application-services`）和 `core-graphics`。需要测一下 release 二进制增加多少体积，目标是不超过 1.5 MB。

## 11. 测试

- **单元测试**：界面树剪枝和文本格式化、`state_id` 过期检测、坐标换算、禁止名单、审批 scope。
- **集成测试（macOS，CI 中跳过，本地 `--ignored` 跑）**：用一个测试用的 AppKit 小程序作为目标，覆盖读树、点击按钮、输入文本、设值、截图尺寸，以及“后台窗口不被激活”。
- **手工验收**：Notes 写一条笔记、Calendar 新建日程、Finder 重命名文件、Safari 在后台搜索；同时人在前台用别的 App，确认焦点和鼠标不受影响。

## 12. 待定问题

1. 对 Electron 和 Chromium 类 App（Slack、VS Code），AX 树往往很稀疏，需要先调用 `AXManualAccessibility` / `AXEnhancedUserInterface` 才能拿到完整的树。要实测这对性能和副作用的影响。
2. `CGEventPostToPid` 的鼠标事件在部分 App 上不生效，需要实测常见 App 的覆盖率，再决定 `needs_foreground` 的默认策略。
3. 全局停止快捷键需要监听全局事件，确认辅助功能授权是否足够，还是需要额外的输入监控（Input Monitoring）授权。
4. 是否需要“只看不动”的模式（只开放 `list_apps` 和 `get_state`），给 `read_only` 之上、比 `ask` 更窄的场景使用。
