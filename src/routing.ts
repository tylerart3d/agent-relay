import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { useEffect, useRef, useState } from "react";

export interface ApprovedModel { model: string; tools: boolean }
export interface ClientPolicy { mode: "pinned" | "auto"; models: ApprovedModel[] | null; minimum_context: number }
export interface RoutingConfig { reclaimable_models?: string[]; models: ApprovedModel[]; clients: Record<string, ClientPolicy> }
export const defaultPolicy: ClientPolicy = { mode: "pinned", models: null, minimum_context: 65536 };
export const canonicalClient = (client: string) => client === "opencode_cli" ? "opencode" : client;

export function useRouting() {
  const revision = useRef(0);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [routing, setRouting] = useState<RoutingConfig>({ models: [], clients: {} });
  const [waiting, setWaiting] = useState<Record<string, number>>({});
  const [choices, setChoices] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    const refresh = async () => {
      const started = revision.current;
      try {
        const [next, recent, queued] = await Promise.all([
          invoke<RoutingConfig>("get_routing_config"), invoke<Record<string, string>>("get_route_choices"), invoke<Record<string, number>>("get_routing_waiting"),
        ]);
        if (active && next && started === revision.current) { setLoaded(true); setRouting(next); setChoices(recent ?? {}); setWaiting(queued ?? {}); setError(null); }
      } catch (e) { if (active) setError(String(e)); }
    };
    void refresh();
    const timer = setInterval(() => { void refresh(); }, 5000);
    const listener = listen("routing-changed", () => { void refresh(); });
    return () => { active = false; clearInterval(timer); void listener.then((stop) => stop()); };
  }, []);
  const save = async (next: RoutingConfig) => {
    revision.current += 1;
    setSaving(true);
    try {
      await invoke("set_routing_config", { routing: next });
      revision.current += 1;
      setRouting(next);
      await emit("routing-changed");
    } finally { setSaving(false); }
  };
  return { routing, choices, waiting, save, error, loaded, saving };
}
