import type { ModelProfile, ReasoningEffort } from "./fleet";

type Thinking = NonNullable<ModelProfile["inference_controls"]["thinking"]>;

// Mirrors ThinkingControls::budget_for in src-tauri/src/domain.rs: the reasoning
// limit is fixed per effort level, never chosen by the user.
const STANDARD: Partial<Record<ReasoningEffort, number>> = {
  minimal: 512,
  low: 2_048,
  medium: 8_192,
  high: 16_384,
};

/** Token limit for an effort level; -1 or Strata's 0 mean unlimited, null means none. */
export function effortBudget(thinking: Thinking, effort: ReasoningEffort | null): number | null {
  if (!effort) return thinking.default_budget ?? null;
  if (effort === "off") return null;
  const configured = thinking.effort_budgets?.[effort];
  if (configured !== undefined) return configured;
  const min = thinking.budget_min;
  const max = thinking.budget_max;
  if (min === null || min === undefined || max === null || max === undefined) return null;
  const unlimited = thinking.adapter === "strata" ? 0 : min < 0 ? -1 : max;
  if (effort === "on") return thinking.default_budget ?? unlimited;
  const standard = STANDARD[effort];
  return standard === undefined ? unlimited : Math.min(Math.max(standard, Math.max(min, 1)), max);
}

export function formatBudget(thinking: Thinking, budget: number | null): string {
  if (budget === null) return "no token limit";
  const unlimited = thinking.adapter === "strata" ? budget === 0 : budget < 0;
  return unlimited ? "unlimited reasoning" : `${budget.toLocaleString()} reasoning tokens`;
}

/** The levels a harness can save; every model maps them to its own. */
export const HARNESS_EFFORTS: ReasoningEffort[] = ["off", "low", "medium", "high"];

const RANK: Partial<Record<ReasoningEffort, number>> = {
  minimal: 0,
  low: 1,
  medium: 2,
  high: 3,
  xhigh: 4,
  max: 5,
};

// Mirrors ThinkingControls::resolve_effort: nearest supported level, ties up.
export function resolveEffort(thinking: Thinking, requested: ReasoningEffort): ReasoningEffort {
  const supported = thinking.efforts;
  if (supported.length === 0 || supported.includes(requested)) return requested;
  const graded = supported.filter((effort) => RANK[effort] !== undefined);
  const rank = RANK[requested];
  if (rank !== undefined && graded.length > 0) {
    return graded.reduce((best, effort) => {
      const distance = Math.abs(RANK[effort]! - rank);
      const bestDistance = Math.abs(RANK[best]! - rank);
      return distance < bestDistance || (distance === bestDistance && RANK[effort]! > RANK[best]!)
        ? effort
        : best;
    });
  }
  if (rank !== undefined && supported.includes("on")) return "on";
  const top = graded.reduce<ReasoningEffort | null>(
    (best, effort) => (best === null || RANK[effort]! > RANK[best]! ? effort : best),
    null,
  );
  const bottom = graded.reduce<ReasoningEffort | null>(
    (best, effort) => (best === null || RANK[effort]! < RANK[best]! ? effort : best),
    null,
  );
  if (requested === "on" && graded.length > 0) {
    const fallback = thinking.default_effort;
    return fallback && RANK[fallback] !== undefined ? fallback : top ?? requested;
  }
  if (requested === "off" && bottom) return bottom;
  return thinking.default_effort ?? requested;
}

export function effortLabel(effort: ReasoningEffort): string {
  return effort === "xhigh" ? "Extra high" : effort[0].toUpperCase() + effort.slice(1);
}
