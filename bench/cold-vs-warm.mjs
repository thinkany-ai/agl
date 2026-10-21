// Cold (one CLI process per turn + --resume) vs agl warm sessions.
// Usage: AGL_HOME=<home with a running daemon> W=<empty work dir> node bench/cold-vs-warm.mjs
// WARM_ONLY=1 skips the cold runs.
import { spawn } from "node:child_process";
import fs from "node:fs";
const W = process.env.W, B = process.env.B, ROUNDS = 4;
const PROMPTS = ["Reply with exactly: one", "Reply with exactly: two", "Reply with exactly: three", "Reply with exactly: four"];
const env = { ...process.env }; for (const k of Object.keys(env)) if (k.startsWith("CLAUDE_CODE_") || k === "CLAUDECODE" || k === "ANTHROPIC_API_KEY") delete env[k];
function runCold(cmd, args, isText) {
  return new Promise((resolve) => {
    const t0 = performance.now(); let first = null, id = null;
    const p = spawn(cmd, args, { cwd: W, env, stdio: ["ignore", "pipe", "ignore"] });
    let buf = "";
    p.stdout.on("data", (d) => { buf += d; let i; while ((i = buf.indexOf("\n")) >= 0) { const line = buf.slice(0, i); buf = buf.slice(i + 1);
      try { const o = JSON.parse(line); id ??= o.session_id || o.thread_id; if (first == null && isText(o)) first = performance.now() - t0; } catch {} } });
    p.on("close", () => resolve({ first_text_ms: Math.round(first), total_ms: Math.round(performance.now() - t0), id }));
  });
}
const claudeText = (o) => o.type === "stream_event" && o.event?.delta?.type === "text_delta";
const codexText = (o) => o.type === "item.completed" && o.item?.type === "agent_message";
const out = {};
// cold claude: one process per turn, --resume
if (!process.env.WARM_ONLY) { const rows = []; let id = null;
  for (let i = 0; i < ROUNDS; i++) {
    const args = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--model", "haiku", "--dangerously-skip-permissions", ...(id ? ["--resume", id] : []), PROMPTS[i]];
    const r = await runCold("claude", args, claudeText); id ??= r.id; rows.push(r);
  } out.claude_cold = rows; console.log("claude cold", JSON.stringify(rows)); }
// cold codex: codex exec / exec resume per turn
if (!process.env.WARM_ONLY) { const rows = []; let id = null;
  for (let i = 0; i < ROUNDS; i++) {
    const args = id ? ["exec", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox", "resume", id, PROMPTS[i]] : ["exec", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox", PROMPTS[i]];
    const r = await runCold("codex", args, codexText); id ??= r.id; rows.push(r);
  } out.codex_cold = rows; console.log("codex cold", JSON.stringify(rows)); }
// agl warm
const info = JSON.parse(fs.readFileSync(`${process.env.AGL_HOME}/daemon.json`, "utf8"));
const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?token=${info.token}`);
let n = 0; const pend = new Map(); const ls = new Set();
const call = (method, params) => new Promise((res, rej) => { const id = String(++n); pend.set(id, { res, rej }); ws.send(JSON.stringify({ type: "request", id, method, params })); });
ws.onmessage = (m) => { const msg = JSON.parse(m.data); if (msg.type === "response") { const p = pend.get(msg.id); pend.delete(msg.id); msg.error ? p.rej(new Error(msg.error)) : p.res(msg.result); } else for (const l of ls) l(msg); };
await new Promise((r) => (ws.onopen = r));
for (const [agent, model] of [["claude", "haiku"], ["codex", null]]) {
  const s = await call("session.create", { agent, model, cwd: W, access: "full", warm: true });
  await new Promise((r) => setTimeout(r, 4000));
  const rows = [];
  for (let i = 0; i < ROUNDS; i++) {
    const t0 = performance.now(); let first = null;
    const t = await call("session.prompt", { session_id: s.id, text: PROMPTS[i] });
    const ended = await new Promise((resolve) => { const l = (msg) => { if (msg.session_id !== s.id) return; const e = msg.event;
      if (e.kind === "text_delta" && first == null && e.text.trim()) first = performance.now() - t0;
      if (e.kind === "turn_ended" && e.turn_id === t.turn_id) { ls.delete(l); resolve(e); } }; ls.add(l); });
    rows.push({ first_text_ms: Math.round(first), total_ms: Math.round(performance.now() - t0), outcome: ended.outcome });
  }
  out[`${agent}_agl_warm`] = rows; console.log(agent, "agl warm", JSON.stringify(rows));
}
fs.writeFileSync(`${process.env.W}/../bench-${process.env.WARM_ONLY ? 'warm' : 'all'}.json`, JSON.stringify(out, null, 2));
process.exit(0);
