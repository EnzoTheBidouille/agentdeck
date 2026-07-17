import { useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

type Status = "idle" | "done" | "error" | "working" | "blocked";

interface Node {
  id: string;
  parent: string | null;
  agent_type: string | null;
  cwd: string | null;
  status: Status;
  started_at_ms: number;
  age_ms: number;
  tokens: number | null;
  duration_ms: number | null;
  tool_calls: number | null;
  model: string | null;
}

interface DeckState {
  connected: boolean;
  aggregate: Status | "unreachable";
  nodes: Node[];
}

const EMPTY: DeckState = { connected: false, aggregate: "unreachable", nodes: [] };

const STATUS_LABEL: Record<Status, string> = {
  blocked: "blocked",
  working: "working",
  error: "error",
  done: "done",
  idle: "idle",
};

function fmtTokens(t: number): string {
  return t >= 1000 ? `${(t / 1000).toFixed(1)}k tok` : `${t} tok`;
}

function fmtDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
}

function fmtAge(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

function basename(path: string | null): string {
  if (!path) return "session";
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? "session";
}

function NodeRow({ node, depth, now, receivedAt }: {
  node: Node;
  depth: number;
  now: number;
  receivedAt: number;
}) {
  const age = node.age_ms + Math.max(0, now - receivedAt);
  const label = depth === 0 ? basename(node.cwd) : node.agent_type ?? "agent";
  const finished = node.status === "done" || node.status === "error";
  return (
    <div className="row" style={{ paddingLeft: 12 + depth * 18 }}>
      <span className={`dot ${node.status}`} />
      <span className="label" title={node.cwd ?? node.id}>{label}</span>
      <span className="meta">
        {node.tokens != null && <span>{fmtTokens(node.tokens)}</span>}
        {node.duration_ms != null && finished
          ? <span>{fmtDuration(node.duration_ms)}</span>
          : <span>{fmtAge(age)}</span>}
        <span className={`status ${node.status}`}>{STATUS_LABEL[node.status]}</span>
      </span>
    </div>
  );
}

export default function App() {
  const [state, setState] = useState<DeckState>(EMPTY);
  const [receivedAt, setReceivedAt] = useState(Date.now());
  const [now, setNow] = useState(Date.now());
  const [installMsg, setInstallMsg] = useState<string | null>(null);

  const installHooks = () => {
    setInstallMsg("Installing…");
    invoke<string>("install_hooks")
      .then((msg) => setInstallMsg(msg))
      .catch((err) => setInstallMsg(String(err)));
  };

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen<DeckState>("state", (e) => {
      setState(e.payload);
      setReceivedAt(Date.now());
    }).then((fn) => (unlisten = fn));
    invoke<DeckState>("get_state").then((s) => {
      setState(s);
      setReceivedAt(Date.now());
    }).catch(() => {});
    const tick = setInterval(() => setNow(Date.now()), 1000);
    return () => {
      unlisten?.();
      clearInterval(tick);
    };
  }, []);

  // parent -> children, then walk depth-first from roots so nesting renders
  // at real depth (subagents can spawn subagents).
  const rows = useMemo(() => {
    const byParent = new Map<string | null, Node[]>();
    const ids = new Set(state.nodes.map((n) => n.id));
    for (const n of state.nodes) {
      // A parent that vanished (reaped) would hide the child: treat as root.
      const key = n.parent && ids.has(n.parent) ? n.parent : null;
      byParent.set(key, [...(byParent.get(key) ?? []), n]);
    }
    const out: { node: Node; depth: number }[] = [];
    const walk = (parent: string | null, depth: number) => {
      for (const n of byParent.get(parent) ?? []) {
        out.push({ node: n, depth });
        walk(n.id, depth + 1);
      }
    };
    walk(null, 0);
    return out;
  }, [state.nodes]);

  return (
    <main className="deck">
      <header className={`bar ${state.connected ? state.aggregate : "unreachable"}`}>
        <span className="title">agentdeck</span>
        <span className="agg">{state.connected ? state.aggregate : "daemon unreachable"}</span>
      </header>
      {!state.connected ? (
        <div className="empty">
          <p>Daemon unreachable.</p>
          <p className="hint">Start it with <code>cargo run --bin agentdeck-daemon</code></p>
        </div>
      ) : rows.length === 0 ? (
        <div className="empty">
          <p>No agents running.</p>
          <p className="hint">Sessions appear here as soon as Claude Code fires a hook.</p>
        </div>
      ) : (
        <div className="tree">
          {rows.map(({ node, depth }) => (
            <NodeRow key={node.id} node={node} depth={depth} now={now} receivedAt={receivedAt} />
          ))}
        </div>
      )}
      <footer className="footer">
        {installMsg
          ? <span className="install-msg" title={installMsg}>{installMsg}</span>
          : <button className="install" onClick={installHooks}>Install hooks</button>}
      </footer>
    </main>
  );
}
