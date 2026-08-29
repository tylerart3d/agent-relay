export interface ModelOptionState {
  loaded: boolean;
  cachedLoaded: boolean;
  selectedInHermes: boolean;
  disabled: boolean;
}

export function getModelOptionState(
  online: boolean,
  hostId: string,
  modelId: string,
  loadedModelId: string | null,
  hermesSelectedModel: string | null,
): ModelOptionState {
  const matchesLoadedModel = loadedModelId === modelId;
  return {
    loaded: online && matchesLoadedModel,
    cachedLoaded: !online && matchesLoadedModel,
    selectedInHermes: hermesSelectedModel === `${hostId}/${modelId}`,
    disabled: !online,
  };
}

interface OffsetNode {
  offsetParent: unknown;
  offsetTop: number;
  parentElement?: unknown;
}

function isOffsetNode(value: unknown): value is OffsetNode {
  return Boolean(
    value &&
      typeof value === "object" &&
      "offsetTop" in value &&
      typeof value.offsetTop === "number" &&
      "offsetParent" in value,
  );
}

/** Returns layout Y without CSS transforms applied by the opening animation. */
export function layoutOffsetTop(element: OffsetNode): number {
  let top = 0;
  let current: unknown = element;
  while (isOffsetNode(current)) {
    top += current.offsetTop;
    current = current.offsetParent;
  }
  current = element.parentElement;
  while (current && typeof current === "object") {
    if ("scrollTop" in current && typeof current.scrollTop === "number") {
      top -= current.scrollTop;
    }
    current = "parentElement" in current ? current.parentElement : null;
  }
  return top;
}

export async function dispatchTrayAction(
  action: string,
  emitAction: (action: string) => Promise<void>,
  hideMenus: () => Promise<void>,
): Promise<void> {
  await emitAction(action);
  await hideMenus();
}
