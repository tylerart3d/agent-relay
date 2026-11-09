import { useState } from "react";
import type { FleetSnapshot } from "./fleet";
import { defaultPolicy, type ApprovedModel, type RoutingConfig } from "./routing";

const clients = ["hermes", "hermes_cli", "opencode", "openbot", "codex", "claude_code", "pi", "copilot", "vscode"];

export function AutoRoutingSettings({ fleet, routing, save }: {
  fleet: FleetSnapshot | null; routing: RoutingConfig; save: (next: RoutingConfig) => Promise<void>;
}) {
  const [draft, setDraft] = useState(routing);
  const [scope, setScope] = useState("shared");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const policy = draft.clients[scope] ?? defaultPolicy;
  const shared = scope === "shared";
  const inherited = !shared && policy.models === null;
  const models = shared || inherited ? draft.models : policy.models ?? [];
  const updateModels = (models: ApprovedModel[]) => setDraft(shared
    ? { ...draft, models }
    : { ...draft, clients: { ...draft.clients, [scope]: { ...policy, models } } });
  const updatePolicy = (updates: Partial<typeof policy>) => setDraft({ ...draft,
    clients: { ...draft.clients, [scope]: { ...policy, ...updates } },
  });
  const catalog = fleet?.hosts.flatMap((host) => host.models.filter((m) => m.kind === "text" && m.capabilities.some((c) => ["chat", "responses", "completions", "anthropic_messages"].includes(c)))
    .map((model) => ({ id: `${host.id}/${model.id}`, label: `${host.display_name} · ${model.display_name}` }))) ?? [];
  const available = catalog.filter((m) => !models.some((entry) => entry.model === m.id));
  return <section className="auto-routing-settings" aria-label="Auto routing settings">
    <p>Use approved loaded models first. Cold starts use idle machines, or drain explicitly reclaimable background models. Auto avoids Windows machines busy with other GPU apps. Switching never interrupts a reply.</p>
    <label>Routing policy<select aria-label="Routing policy" value={scope} onChange={(e) => setScope(e.target.value)}>
      <option value="shared">Shared approved models</option>
      {clients.map((id) => <option key={id} value={id}>{id.replace(/_/g, " ")}{id === "opencode" ? " (GUI + CLI)" : ""}</option>)}
    </select></label>
    {!shared && <>
      <label>Mode<select aria-label="Routing mode" value={policy.mode} onChange={(e) => updatePolicy({ mode: e.target.value as "pinned" | "auto" })}>
        <option value="pinned">Pinned model</option><option value="auto">Auto</option>
      </select></label>
      <label>Minimum context<select aria-label="Auto minimum context" value={policy.minimum_context} onChange={(e) => updatePolicy({ minimum_context: Number(e.target.value) })}>
        {[16384,32768,65536,131072,262144,524288,1048576].map((n) => <option key={n} value={n}>{n/1024}K</option>)}
      </select></label>
      <label><input type="checkbox" checked={inherited} onChange={(e) => updatePolicy({ models: e.target.checked ? null : [...draft.models] })} />Use shared approved models</label>
    </>}
    <ol className="auto-approved-list">
      {models.map((entry, index) => <li key={entry.model}>
        <span>{catalog.find((m) => m.id === entry.model)?.label ?? entry.model}</span>
        <div>
          <label><input type="checkbox" checked={entry.tools} disabled={inherited} onChange={(e) => updateModels(models.map((m, i) => i === index ? { ...m, tools: e.target.checked } : m))} />Tools tested</label>
          <button aria-label={`Move ${entry.model} up`} disabled={inherited || index === 0} onClick={() => { const copy = [...models]; [copy[index-1],copy[index]] = [copy[index],copy[index-1]]; updateModels(copy); }}>↑</button>
          <button aria-label={`Remove ${entry.model}`} disabled={inherited} onClick={() => updateModels(models.filter((_, i) => i !== index))}>Remove</button>
        </div>
      </li>)}
    </ol>
    {!models.length && <p>No models approved. Auto will return a clear unavailable error.</p>}
    <label>Add approved model<select aria-label="Add approved model" value="" disabled={inherited} onChange={(e) => { if (e.target.value) updateModels([...models, { model: e.target.value, tools: false }]); }}>
      <option value="">Choose machine and model…</option>{available.map((m) => <option key={m.id} value={m.id}>{m.label}</option>)}
    </select></label>
    <small>Approve tools only for models you have tested. Unknown context sizes are excluded. Set the minimum context at least as high as your harness context setting.</small>
    {fleet?.hosts.find((h) => h.id === fleet.local_host_id)?.models.some((m) => m.priority === "background") &&
      <p>Profiles marked priority: background in runtime metadata also yield automatically. Edit their source metadata to remove that designation.</p>}
    <fieldset><legend>Reclaimable background models on this machine</legend>
      <small>Interactive Auto requests allow 90 seconds to drain, then cancel remaining background work. Background requests receive 503 until the foreground model unloads. Configure each serving machine separately.</small>
      {catalog.filter((m) => m.id.startsWith(`${fleet?.local_host_id}/`)).map((m) => <label key={m.id}>
        <input type="checkbox" aria-label={`Allow reclaim ${m.id}`} checked={(draft.reclaimable_models ?? []).includes(m.id)}
          onChange={(e) => setDraft({ ...draft, reclaimable_models: e.target.checked
            ? [...(draft.reclaimable_models ?? []), m.id] : (draft.reclaimable_models ?? []).filter((id) => id !== m.id) })} />{m.label}
      </label>)}
    </fieldset>
    <button disabled={busy} onClick={async () => { setBusy(true); try { await save(draft); setMessage("Routing saved"); } catch (e) { setMessage(String(e)); } finally { setBusy(false); } }}>Save routing</button>
    {message && <p role="status">{message}</p>}
  </section>;
}
