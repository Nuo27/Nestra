import type { ModelAbilities, Modalities } from "../ipc";

/** Resolved value + provenance for one capability field on one model row.
 *  Used by the disclosure to label each toggle with its source. */
type AbilityRow = {
  /** What the writer will actually emit (override if set, else default). */
  effective?: boolean | undefined;
  /** Whether the value is from the user's override map. */
  overridden: boolean;
};

export function resolveField(
  def: ModelAbilities | undefined,
  ov: ModelAbilities | undefined,
  key: keyof Pick<ModelAbilities, "reasoning" | "tool_call" | "attachment" | "temperature">,
): AbilityRow {
  const dv = def?.[key];
  const ovv = ov?.[key];
  if (ovv !== undefined) return { effective: ovv, overridden: true };
  if (dv !== undefined) return { effective: dv, overridden: false };
  return { effective: undefined, overridden: false };
}

export function resolveLimit(
  def: ModelAbilities | undefined,
  ov: ModelAbilities | undefined,
): { effective?: { context: number; output: number }; overridden: boolean } {
  if (ov?.limit) return { effective: ov.limit, overridden: true };
  if (def?.limit) return { effective: def.limit, overridden: false };
  return { effective: undefined, overridden: false };
}

/**
 * Resolve the modalities matrix the writer will emit. Override wins when
 * set (even partially — the override struct replaces the default wholesale,
 * matching `merge_field_overrides` semantics); otherwise the merged default
 * surfaces through. The disclosure renders this read-only — there is no
 * per-modality editor is not included; only the JSON-override escape hatch.
 */
export function resolveModalities(
  def: ModelAbilities | undefined,
  ov: ModelAbilities | undefined,
): { effective?: Modalities; overridden: boolean } {
  if (ov?.modalities) return { effective: ov.modalities, overridden: true };
  if (def?.modalities) return { effective: def.modalities, overridden: false };
  return { effective: undefined, overridden: false };
}
