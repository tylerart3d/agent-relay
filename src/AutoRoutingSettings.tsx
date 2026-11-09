import { useState } from "react";
import type { FleetSnapshot } from "./fleet";
import { defaultPolicy, type ApprovedModel, type RoutingConfig } from "./routing";

const clients = ["hermes", "hermes_cli", "opencode", "openbot", "codex", "claude_code", "pi", "copilot", "vscode"];

// Every control saves immediately from the latest saved routing. A local draft would
// silently drop unsaved edits and could overwrite changes made from another window.
export function AutoRoutingSettings({ fleet, routing, save }: {
  fleet: FleetSnapshot | null; routing: RoutingConfig; save: (next: RoutingConfig) => Promise<void>;
}) {
  const [scope, setScope] = useState("shared");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const policy = routing.clients[scope] ?? defaultPolicy;
  const shared = scope === "shared";
  const inherited = !shared && policy.models === null;
  const models = shared || inherited ? routing.models : policy.models ?? [];
  const commit = async (next: RoutingConfig) => {
    setBusy(true);
    try { await save(next); setMessage("Routing saved"); } catch (e) { setMessage(String(e)); } finally { setBusy(false); }
  };
  const updateModels = (models: ApprovedModel[]) => commit(shared
    ? { ...routing, models }
    : { ...routing, clients: { ...routing.clients, [scope]: { ...policy, models } } });
  const updatePolicy = (updates: Partial<typeof policy>) => commit({ ...routing,
    clients: { ...routing.clients, [scope]: { ...policy, ...updates } },
  });
  const catalog = fleet?.hosts.flatMap((host) => host.models.filter((m) => m.kind === "text" && m.capabilities.some((c) => ["chat", "responses", "completions", "anthropic_messages"].includes(c)))
    .map((model) => ({ id: `${host.id}/${model.id}`, label: `${host.display_name} · ${model.display_name}` }))) ?? [];
  const available = catalog.filter((m) => !models.some((entry) => entry.model === m.id));
  const reclaimable = routing.reclaimable_models ?? [];
  return <section className="auto-routing-settings" aria-label="Auto routing settings">
    <p>Use approved loaded models first. Cold starts use idle machines, or drain explicitly reclaimable background models. Auto avoids Windows machines busy with other GPU apps. Switching never interrupts a reply. Changes save immediately.</p>
    <label>Routing policy<select aria-label="Routing policy" value={scope} onChange={(e) => setScope(e.target.value)}>
      <option value="shared">Shared approved models</option>
      {clients.map((id) => <option key={id} value={id}>{id.replace(/_/g, " ")}{id === "opencode" ? " (GUI + CLI)" : ""}</option>)}
    </select></label>
    {!shared && <>
      <label>Mode<select aria-label="Routing mode" disabled={busy} value={policy.mode} onChange={(e) => updatePolicy({ mode: e.target.value as "pinned" | "auto" })}>
        <option value="pinned">Pinned model</option><option value="auto">Auto</option>
      </select></label>
      <label>Minimum context<select aria-label="Auto minimum context" disabled={busy} value={policy.minimum_context} onChange={(e) => updatePolicy({ minimum_context: Number(e.target.value) })}>
        {[16384,32768,65536,131072,262144,524288,1048576].map((n) => <option key={n} value={n}>{n/1024}K</option>)}
      </select></label>
      <label><input type="checkbox" disabled={busy} checked={inherited} onChange={(e) => updatePolicy({ models: e.target.checked ? null : [...routing.models] })} />Use shared approved models</label>
      <label><input type="checkbox" aria-label="Prefer listed order" disabled={busy} checked={policy.prefer_order ?? false} onChange={(e) => updatePolicy({ prefer_order: e.target.checked })} />Prefer listed order (return to the first eligible model instead of staying on a loaded one)</label>
      <label><input type="checkbox" aria-label="One request at a time" disabled={busy} checked={policy.max_concurrent === 1} onChange={(e) => updatePolicy({ max_concurrent: e.target.checked ? 1 : null })} />One request at a time (later requests wait for the running one, up to 10 minutes)</label>
    </>}
    <ol className="auto-approved-list">
      {models.map((entry, index) => <li key={entry.model}>
        <span>{catalog.find((m) => m.id === entry.model)?.label ?? entry.model}</span>
        <div>
          <label><input type="checkbox" checked={entry.tools} disabled={busy || inherited} onChange={(e) => updateModels(models.map((m, i) => i === index ? { ...m, tools: e.target.checked } : m))} />Tools tested</label>
          <button aria-label={`Move ${entry.model} up`} disabled={busy || inherited || index === 0} onClick={() => { const copy = [...models]; [copy[index-1],copy[index]] = [copy[index],copy[index-1]]; void updateModels(copy); }}>↑</button>
          <button aria-label={`Remove ${entry.model}`} disabled={busy || inherited} onClick={() => updateModels(models.filter((_, i) => i !== index))}>Remove</button>
        </div>
      </li>)}
    </ol>
    {!models.length && <p>No models approved. Auto will return a clear unavailable error.</p>}
    <label>Add approved model<select aria-label="Add approved model" value="" disabled={busy || inherited} onChange={(e) => { if (e.target.value) void updateModels([...models, { model: e.target.value, tools: false }]); }}>
      <option value="">Choose machine and model…</option>{available.map((m) => <option key={m.id} value={m.id}>{m.label}</option>)}
    </select></label>
    <small>Approve tools only for models you have tested. Unknown context sizes are excluded. Set the minimum context at least as high as your harness context setting.</small>
    {fleet?.hosts.find((h) => h.id === fleet.local_host_id)?.models.some((m) => m.priority === "background") &&
      <p>Profiles marked priority: background in runtime metadata also yield automatically. Edit their source metadata to remove that designation.</p>}
    <fieldset><legend>Reclaimable background models on this machine</legend>
      <small>Interactive Auto requests allow 90 seconds to drain, then cancel remaining background work. Background requests receive 503 until the foreground model unloads. An idle foreground model is released after background work has waited 3 minutes. Configure each serving machine separately.</small>
      {catalog.filter((m) => m.id.startsWith(`${fleet?.local_host_id}/`)).map((m) => <label key={m.id}>
        <input type="checkbox" aria-label={`Allow reclaim ${m.id}`} disabled={busy} checked={reclaimable.includes(m.id)}
          onChange={(e) => commit({ ...routing, reclaimable_models: e.target.checked
            ? [...reclaimable, m.id] : reclaimable.filter((id) => id !== m.id) })} />{m.label}
      </label>)}
    </fieldset>
    {message && <p role="status">{message}</p>}
  </section>;
}
