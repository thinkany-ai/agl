# 产品接入指南

接入文档已经整理成给编码 Agent 用的 skill，同时也适合人直接阅读：

- [skills/agl-integration/SKILL.md](../skills/agl-integration/SKILL.md)：先判断该用哪种接入方式，再按通用步骤做，最后对照验收清单
- [桌面 App](../skills/agl-integration/references/desktop-app.md)：sidecar 打包、Rust 进程内调用、共享 daemon，以及以自己的名义嵌入（见下文）
- [Web 产品](../skills/agl-integration/references/web-product.md)：配对码、安装脚本、机器链路、多实例网关、安全清单
- [已有运行时](../skills/agl-integration/references/existing-runtime.md)：只替换执行层的迁移步骤
- [协议参考](../skills/agl-integration/references/protocol.md)：方法、事件、权限档位、配置项
- [方案对比与实测](comparison.md)
- [设计草案：内置 computer-use 与 browser-use](computer-use.md)

## 嵌入与品牌化（agl 0.3.17 起）

产品可以用自己的名字发布 agl，并把整个 daemon（hub 链接、会话、内置工具）放进自己的进程里，不再依赖 sidecar 或共享的 `~/.agl`。做法是在 `main` 最开始调用一次 `agl::brand::set(Brand { … })`，之后使用和 agl 自身相同的接口：

| 字段 | 作用 |
|---|---|
| `cli_name` / `display_name` | 命令行用法和提示文字里的名字 |
| `home` | 默认状态目录；`AGL_HOME` 仍然优先 |
| `service_label` / `systemd_unit` | 开机自启服务的 launchd 标签和 systemd 单元名 |
| `releases` | 自更新的发布地址（与 GitHub Releases 同样的布局）；为 `None` 时随 App 更新，`update` 与 `daemon.update` 拒绝 |
| `allowed_hubs` | 允许连接的 hub URL 前缀；其他地址拒绝连接，已在配置里的不建立链接 |
| `computer_helper` | 自己签名的 computer-use 辅助程序；macOS 的辅助功能、屏幕录制授权记在它的 App 名下，升级后不失效 |
| `blocked_apps` | computer use 始终不操作的 bundle id（例如产品自己的 App） |
| `product` | 在机器 `hello` 里报告的 `{name, version}` |

- 自己的命令行：`agl::brand::set(..)` 后调用 `agl::app::hold_from_args()` 与 `agl::cli::run()`。
- App 内常驻：`agl::daemon::spawn_embedded(cfg, opts)` 在调用方的 tokio 运行时里启动 daemon，返回端口与 token；退出前看 `busy()`，再 `shutdown().await`。同一个状态目录同时只能有一个 daemon。
- 链接管理：`agl::link::connect(url, ConnectOptions { … })`、`agl::link::disconnect(url)`，效果与 `connect` / `disconnect` 命令相同。
- 辅助程序的 `main` 只需要 `std::process::exit(agl::computer::helper_main())`；agl 以 `<helper> __computer --responsible` 启动它，它会先让自己成为“负责进程”再开始服务。

详见 [desktop-app.md](../skills/agl-integration/references/desktop-app.md#embedding-and-branding-agl-0317)。

### 宿主注册的工具（agl 0.3.17 起）

嵌入 agl 的应用可以在内置工具之外向 hub 提供自己的工具：实现 `agl::machine::ToolProvider`，在 daemon 启动前调用
`agl::machine::register_provider` 注册。

- **列出**：这些工具排在内置工具之后出现在 `tools.list` 和机器 `hello` 的 `tools` 中；每次列出都会调用 `tools()`。
- **谁能调用**：与 computer use 规则相同。`full` 链接和本机客户端可以直接调用；`ask` / `edit` 链接须传 `approvals: true`，
  每个工具在每个 `session` 中第一次使用时等待用户确认（`allow_session` 记住）；`read_only` 不提供。
- **调用**：`arguments` 原样交给提供方（包括 hub 附加的键；`_agl` 为保留键）。`CallContext` 带有 `session`、
  链接（`origin_key`，即 hub 地址，本机客户端为空）和 `images`。提供方返回的错误即 `tools.call` 的错误，消息原样保留。
- **图片**：结果中的 `images: [{mime_type, data}]`（base64）按 computer use 截图的方式处理：只有 `images: true` 时返回；
  提供 `image_upload` 时 PUT 上传（结果为 `{uploaded: true, bytes}`，失败则内联返回并带 `image_upload_failed` 警告）；
  与同一 session、同一位置上次发出的图片相同时返回 `{unchanged: true}`。位置取结果中的 `image_key`（返回前移除），否则为工具名。
- **名称**不能与内置工具（包括任何系统上的 computer use 工具）或其他提供方的工具重复，否则 `register_provider` 失败。
