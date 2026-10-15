export type CliProject = { selected: string | null; recent: string[] };
const key = (client: string) => `agentrelay.cli-project.${client}`;

export function readCliProject(client: string): CliProject {
  try {
    const value = JSON.parse(localStorage.getItem(key(client)) ?? "null");
    return {
      selected: typeof value?.selected === "string" ? value.selected : null,
      recent: Array.isArray(value?.recent)
        ? value.recent.filter((path: unknown): path is string => typeof path === "string").slice(0, 8)
        : [],
    };
  } catch { return { selected: null, recent: [] }; }
}

export function saveCliProject(client: string, selected: string | null): CliProject {
  const previous = readCliProject(client);
  const next = { selected, recent: selected
    ? [selected, ...previous.recent.filter((path) => path !== selected)].slice(0, 8)
    : previous.recent };
  localStorage.setItem(key(client), JSON.stringify(next));
  return next;
}
