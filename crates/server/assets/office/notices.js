// Turns two successive office states into the notifications the bottom-left stack shows:
// a run starting, its step changing, finishing, failing, or stopping for an approval. The
// first state is the baseline and says nothing; loops that appear or vanish say nothing.

const fill = (s, vars) => String(s || "").replace(/\{(\w+)\}/g, (m, k) => (k in vars ? vars[k] : m));

/// `diffLoops(prev, next, strings)` returns `[{loop, kind, text}]`, `kind` one of
/// `started`, `step`, `done`, `failed`, `waiting`. `prev` null means no baseline yet.
export function diffLoops(prev, next, strings = {}) {
  if (!Array.isArray(prev) || !Array.isArray(next)) return [];
  const before = new Map(prev.map((l) => [l.name, l]));
  const out = [];
  for (const l of next) {
    const was = before.get(l.name);
    if (!was) continue;
    const vars = { loop: l.title || l.name, step: l.step || "" };
    const say = (kind, key) => out.push({ loop: l.name, kind, text: fill(strings[key], vars) });
    if (l.state === was.state) {
      if (l.state === "working" && l.step && l.step !== was.step) say("step", "toast_step");
      continue;
    }
    if (l.state === "working") say(l.step ? "step" : "started", l.step ? "toast_step" : "toast_started");
    else if (l.state === "failed") say("failed", "toast_failed");
    else if (l.state === "waiting") say("waiting", "toast_waiting");
    else if (l.state === "idle" && was.state === "working") say("done", "toast_done");
  }
  return out;
}
