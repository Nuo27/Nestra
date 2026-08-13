// Shared error extractor for Tauri invoke rejections.
//
// Tauri commands return JSON-serialized `AppError`s on failure. Without this
// helper, the raw JSON blob (`{"code":"Validation","message":"..."}`) ends up
// in the UI as the error message. `extractError` unwraps the `message` field
// when present and falls back to the raw string.

export function extractError(e: unknown): string | null {
  if (e === null || e === undefined) return null;
  const candidates: string[] = [];
  if (e instanceof Error && e.message) candidates.push(e.message);
  // A plain string must be used VERBATIM — `JSON.stringify("...")` would
  // double-escape it (quotes + backslashes), so a JSON-blob rejection like
  // `{"code":"Validation","message":"x"}` arrives double-encoded and the
  // parse below can never recover `message`.
  if (typeof e === "string" && e) candidates.push(e);
  else {
    try {
      const s = JSON.stringify(e);
      if (s && s !== "{}") candidates.push(s);
    } catch {
      /* unstringifiable */
    }
  }
  for (const raw of candidates) {
    if (!raw || raw === "[object Object]") continue;
    try {
      const parsed = JSON.parse(raw);
      if (parsed && typeof parsed.message === "string") return parsed.message;
    } catch {
      /* not JSON — treat raw as the message */
    }
    return raw;
  }
  return null;
}