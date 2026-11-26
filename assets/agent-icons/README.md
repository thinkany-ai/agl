# Agent icons

`AgentInfo.icon` for the built-in agents (`src/agents/icons.rs`). Each is the
maker's mark and remains its owner's trademark; agl shows it only to name the
agent it drives.

| File | Source |
|---|---|
| `claude.svg` | lobe-icons `claudecode-color` |
| `codex.svg` | lobe-icons `codex-color` |
| `gemini.svg` | lobe-icons `geminicli-color` |
| `antigravity.svg` | lobe-icons `antigravity-color` |
| `kimi.svg` | lobe-icons `kimi-color` |
| `opencode.svg` | lobe-icons `opencode` |
| `grok.svg` | lobe-icons `grok` |
| `openclaw.svg` | lobe-icons `openclaw-color` |
| `hermes.svg` | lobe-icons `hermesagent` |
| `cursor.svg` | lobe-icons `cursor` |
| `pi.svg` | lobe-icons `pi` (the same mark as pi.dev) |
| `droid.svg` | https://factory.ai/favicon.svg |
| `omp.svg` | https://omp.sh/favicon.svg |

lobe-icons: `@lobehub/icons-static-svg` 1.106.0, MIT,
https://github.com/lobehub/lobe-icons.

All were minified with svgo 3.3.2 (`preset-default` keeping `viewBox`,
`removeDimensions`, `removeTitle`, style and class attributes removed, ids
prefixed `agl-<agent>-` so several icons can share a page). To add one: run the
same, name the file after the agent id, add it to `icons::builtin`, and list
it here. `cargo test icons` checks every built-in agent has one.
