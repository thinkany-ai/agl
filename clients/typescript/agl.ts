/**
 * agl client — zero dependencies, works in Node ≥22 (global WebSocket),
 * browsers, Electron, Deno and Bun.
 *
 * Talks to either
 *   - the local daemon:   ws://127.0.0.1:<port>/ws?token=<token>   (daemon.json)
 *   - a hub's UI socket:  wss://hub/ws/ui?token=<admin>  with `machineId`
 *
 * Usage:
 *   const agl = await AglClient.local();                 // Node: reads ~/.agl/daemon.json
 *   const s = await agl.createSession({ agent: "claude", cwd: "/repo", access: "ask" });
 *   const r = await agl.runTurn(s.id, "fix the failing test", {
 *     onEvent: (e) => e.kind === "text_delta" && process.stdout.write(e.text),
 *     onPermission: async (p) => "allow",               // or show a dialog
 *   });
 *   console.log(r.outcome, r.timing);
 */

export type Access = "full" | "edit" | "ask" | "read_only";
export type Decision = "allow" | "allow_session" | "deny";

/** An image a tool returned (base64). `omitted` images carry no data. */
export interface ToolImage { mime_type: string; data: string; omitted?: boolean }

export type AgentEvent =
  | { kind: "status"; status: "idle" | "starting" | "running" | "waiting" | "suspended" | "closed" }
  | { kind: "user_message"; turn_id: string; text: string }
  | { kind: "queued"; turn_id: string; position: number }
  | { kind: "steered"; turn_id: string; into_turn_id: string }
  | { kind: "turn_started"; turn_id: string }
  | { kind: "text_delta"; text: string }
  | { kind: "reasoning"; text: string }
  | { kind: "tool_start"; call_id: string; name: string; input: unknown }
  | { kind: "tool_end"; call_id: string; ok: boolean; output: string; images?: ToolImage[] }
  | { kind: "plan"; entries: unknown }
  | { kind: "permission_request"; request_id: string; tool: string; title: string; input: unknown; options: Decision[]; expires_at: number }
  | { kind: "permission_resolved"; request_id: string; decision: Decision; by: "client" | "policy" | "timeout" | "cancelled" }
  | { kind: "heartbeat"; turn_id: string; elapsed_ms: number; idle_ms: number; stalled: boolean }
  | { kind: "retrying"; message: string }
  | { kind: "turn_ended"; turn_id: string; outcome: "completed" | "failed" | "aborted"; error?: string; usage?: unknown; timing?: Timing }
  | { kind: "info"; native_id?: string; model?: string }
  | { kind: "error"; message: string };

export interface Timing { warm: boolean; launch_ms: number | null; first_event_ms: number | null; first_text_ms: number | null; total_ms: number }

export interface EventEnvelope { session_id: string; seq: number; ts: number; event: AgentEvent; machine_id?: string }

export interface SessionMeta {
  id: string; name?: string; agent: string; cwd: string; model?: string; current_model?: string; effort?: string; access: Access; origin: string;
  native_id?: string; status: string; created_at: number; last_active_at: number; turns: number;
  queued: number; live: boolean; pending_approvals: number; features: string[];
}

export interface InstallInfo {
  /** For an installed agent. */
  source?: "npm" | "bun" | "brew" | "native" | "app-bundle" | "unknown";
  package?: string; prefix?: string;
  /** Shell commands for the user to confirm; agl never runs them. */
  install_command?: string; update_command?: string; guide?: string;
}

export interface AgentInfo {
  id: string; name: string; protocol: string; available: boolean; path?: string; version?: string; features: string[];
  /** Empty from agl ≤0.2. */
  status?: "ready" | "desktop-only" | "not-found";
  desktop_app?: string; install?: InstallInfo;
  /** Only with `check_updates`. */
  latest_version?: string; update?: "available" | "current" | "unknown";
}

export interface CreateSession { agent: string; cwd?: string; model?: string; effort?: string; access?: Access; name?: string; resume?: string; warm?: boolean }

export interface TurnResult { turn_id: string; outcome: "completed" | "failed" | "aborted"; error?: string; usage?: unknown; timing?: Timing; text: string }

type Listener = (e: EventEnvelope) => void;

export class AglError extends Error {}

export class AglClient {
  private ws?: WebSocket;
  private nextId = 1;
  private pending = new Map<string, { resolve: (v: any) => void; reject: (e: Error) => void }>();
  private listeners = new Set<Listener>();
  private subscriptions = new Set<string>();
  private subscribedAll = false;
  private opening?: Promise<void>;
  private closed = false;

  /**
   * @param url       ws(s) URL including the token query parameter
   * @param machineId set when talking to a hub: requests are routed to this machine
   */
  private url: string;
  private opts: { machineId?: string; WebSocket?: typeof WebSocket };

  constructor(url: string, opts: { machineId?: string; WebSocket?: typeof WebSocket } = {}) {
    this.url = url;
    this.opts = opts;
  }

  /** Node/Electron main: connect to the local daemon via $AGL_HOME/daemon.json. */
  static async local(home?: string): Promise<AglClient> {
    const fs = await import("node:fs");
    const os = await import("node:os");
    const path = await import("node:path");
    const dir = home ?? process.env.AGL_HOME ?? path.join(os.homedir(), ".agl");
    const info = JSON.parse(fs.readFileSync(path.join(dir, "daemon.json"), "utf8"));
    const c = new AglClient(`ws://127.0.0.1:${info.port}/ws?token=${encodeURIComponent(info.token)}`);
    await c.connect();
    return c;
  }

  /** Same client for a hub (`/ws/ui`), bound to one machine. */
  forMachine(machineId: string): AglClient {
    const c = new AglClient(this.url, { ...this.opts, machineId });
    c.ws = this.ws;
    c.pending = this.pending;
    c.listeners = this.listeners;
    c.opening = this.opening;
    return c;
  }

  connect(): Promise<void> {
    if (this.opening) return this.opening;
    const WS = this.opts.WebSocket ?? (globalThis as any).WebSocket;
    if (!WS) throw new AglError("no WebSocket implementation; pass opts.WebSocket");
    this.opening = new Promise<void>((resolve, reject) => {
      const ws: WebSocket = new WS(this.url);
      ws.onopen = () => {
        this.ws = ws;
        // Restore subscriptions after a reconnect.
        if (this.subscribedAll) void this.call("subscribe", { all: true }).catch(() => {});
        for (const s of this.subscriptions) void this.call("subscribe", { session_id: s }).catch(() => {});
        resolve();
      };
      ws.onerror = () => reject(new AglError(`cannot connect to ${this.url.replace(/token=[^&]+/, "token=…")}`));
      ws.onclose = () => {
        for (const p of this.pending.values()) p.reject(new AglError("connection closed"));
        this.pending.clear();
        this.opening = undefined;
        if (!this.closed) setTimeout(() => this.connect().catch(() => {}), 1000);
      };
      ws.onmessage = (m: MessageEvent) => this.onMessage(JSON.parse(String(m.data)));
    });
    return this.opening;
  }

  close() {
    this.closed = true;
    this.ws?.close();
  }

  private onMessage(msg: any) {
    if (msg.type === "response") {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      msg.error ? p.reject(new AglError(msg.error)) : p.resolve(msg.result);
    } else if (msg.type === "event") {
      const { type, ...env } = msg;
      for (const l of this.listeners) l(env as EventEnvelope);
    }
  }

  async call<T = any>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    await this.connect();
    const id = String(this.nextId++);
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws!.send(JSON.stringify({ type: "request", id, method, params, machine_id: this.opts.machineId }));
    });
  }

  /** Listen to events (all sessions you subscribed to; on a hub, every event). */
  on(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  async subscribe(sessionId?: string) {
    if (this.opts.machineId) return; // the hub forwards every event
    if (sessionId) {
      this.subscriptions.add(sessionId);
      await this.call("subscribe", { session_id: sessionId });
    } else {
      this.subscribedAll = true;
      await this.call("subscribe", { all: true });
    }
  }

  agents = (opts: { check_updates?: boolean } = {}) => this.call<AgentInfo[]>("agents.list", opts);
  models = (agent: string) => this.call<{ agent: string; models: { id: string; name?: string; efforts?: string[]; default?: boolean }[] }>("agents.models", { agent });
  createSession = (p: CreateSession) => this.call<SessionMeta>("session.create", p as any);
  sessions = () => this.call<SessionMeta[]>("session.list");
  session = (id: string) => this.call<SessionMeta>("session.get", { session_id: id });
  history = (id: string, afterSeq = 0, limit?: number) => this.call<EventEnvelope[]>("session.history", { session_id: id, after_seq: afterSeq, limit });
  prompt = (id: string, text: string, mode: "queue" | "steer" | "interrupt" = "queue", turnId?: string) =>
    this.call<{ session_id: string; turn_id: string }>("session.prompt", { session_id: id, text, mode, turn_id: turnId });
  approve = (id: string, requestId: string, decision: Decision) => this.call("session.approve", { session_id: id, request_id: requestId, decision });
  abort = (id: string) => this.call("session.abort", { session_id: id });
  warm = (id: string) => this.call("session.warm", { session_id: id });
  configure = (id: string, p: { model?: string; effort?: string; access?: Access; name?: string }) => this.call<SessionMeta>("session.configure", { session_id: id, ...p });
  closeSession = (id: string) => this.call("session.close", { session_id: id });

  /**
   * Send a prompt and resolve when its turn ends. Collects the reply text,
   * forwards every event, and answers permission requests via `onPermission`
   * (unanswered requests are denied by agl after its approval timeout).
   */
  async runTurn(
    sessionId: string,
    text: string,
    o: { mode?: "queue" | "steer" | "interrupt"; onEvent?: (e: AgentEvent) => void; onPermission?: (p: Extract<AgentEvent, { kind: "permission_request" }>) => Promise<Decision> | Decision; signal?: AbortSignal } = {},
  ): Promise<TurnResult> {
    await this.subscribe(sessionId);
    const turnId = Math.random().toString(36).slice(2, 14);
    let started = false;
    let reply = "";
    const done = new Promise<TurnResult>((resolve) => {
      const off = this.on(async (env) => {
        if (env.session_id !== sessionId || (this.opts.machineId && env.machine_id && env.machine_id !== this.opts.machineId)) return;
        const e = env.event;
        if (e.kind === "turn_started" && e.turn_id === turnId) started = true;
        if (e.kind === "steered" && e.turn_id === turnId) {
          off();
          return resolve({ turn_id: turnId, outcome: "completed", text: "" });
        }
        if (e.kind === "turn_ended" && e.turn_id === turnId) {
          off();
          o.onEvent?.(e);
          return resolve({ turn_id: turnId, outcome: e.outcome, error: e.error, usage: e.usage, timing: e.timing, text: reply });
        }
        if (!started) return;
        if (e.kind === "text_delta") reply += e.text;
        o.onEvent?.(e);
        if (e.kind === "permission_request" && o.onPermission) {
          const d = await o.onPermission(e);
          await this.approve(sessionId, e.request_id, d).catch(() => {});
        }
      });
    });
    o.signal?.addEventListener("abort", () => void this.abort(sessionId));
    await this.prompt(sessionId, text, o.mode ?? "queue", turnId);
    return done;
  }
}
