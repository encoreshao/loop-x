// Keeps the Office view current: refetches the state on every `snapshot` event of the
// dashboard's SSE stream (EventSource reconnects by itself) and on a slow poll as a
// safety net. After `staleMs` without a successful fetch it calls `onStale()` once;
// the next success calls `onFresh()`. `fetchState()` resolves to the state and rejects
// on failure; `onState(state)` receives each fresh one.

/// `startLive({fetchState, onState, onStale, onFresh, sse, pollMs, staleMs})` returns
/// `{refresh, stop}`: `refresh()` fetches now, `stop()` closes the stream and the timers.
export function startLive({ fetchState, onState, onStale, onFresh, sse = "/api/events", pollMs = 30000, staleMs = 60000 }) {
  let stopped = false, stale = false, busy = false, again = false;
  let lastOk = Date.now();

  async function refresh() {
    // One fetch at a time; a request that arrives meanwhile triggers one more afterwards.
    if (stopped) return;
    if (busy) {
      again = true;
      return;
    }
    busy = true;
    try {
      const state = await fetchState();
      if (stopped) return;
      lastOk = Date.now();
      onState(state);
      if (stale) {
        stale = false;
        if (onFresh) onFresh();
      }
    } catch {
      /* the stale timer reports a failure that lasts */
    } finally {
      busy = false;
      if (again && !stopped) {
        again = false;
        refresh();
      }
    }
  }

  let source = null;
  if (typeof EventSource === "function") {
    try {
      source = new EventSource(sse);
      source.addEventListener("snapshot", refresh);
    } catch {
      source = null;
    }
  }
  const poll = setInterval(refresh, pollMs);
  const watch = setInterval(() => {
    if (!stale && !stopped && Date.now() - lastOk >= staleMs) {
      stale = true;
      if (onStale) onStale();
    }
  }, Math.max(100, Math.min(1000, staleMs / 4)));

  return {
    refresh,
    stop() {
      stopped = true;
      if (source) source.close();
      clearInterval(poll);
      clearInterval(watch);
    },
  };
}
