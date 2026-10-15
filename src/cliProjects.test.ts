// @vitest-environment jsdom
import { beforeEach, expect, it } from "vitest";
import { readCliProject, saveCliProject } from "./cliProjects";
beforeEach(() => localStorage.clear());
it("remembers project folders independently per harness with bounded recents", () => {
  for (let i = 0; i < 10; i++) saveCliProject("codex", `/projects/${i}`);
  const result = saveCliProject("codex", "/projects/5");
  expect(result.recent).toHaveLength(8);
  expect(result.recent[0]).toBe("/projects/5");
  expect(readCliProject("pi").selected).toBeNull();
  expect(readCliProject("codex").selected).toBe("/projects/5");
  expect(saveCliProject("codex", null).recent).toEqual(result.recent);
});
it("recovers from malformed saved preferences", () => {
  localStorage.setItem("agentrelay.cli-project.codex", "broken");
  expect(readCliProject("codex")).toEqual({ selected: null, recent: [] });
});
