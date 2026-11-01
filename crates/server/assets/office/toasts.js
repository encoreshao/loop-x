// The Office view's run notifications. The cards themselves are the dashboard's shared toast
// (`window.loopxToast`, defined by the page shell: same look, same bottom-left stack), so a
// loop's card is just a toast keyed by the loop. Started, step and done cards fade away after
// `ttlMs`; failures and approvals stay until dismissed.

/// `createToasts(root, strings, {max, ttlMs, onOpen})` returns `{push, items, dispose}`.
export function createToasts(_root, _strings = {}, { max = 10, ttlMs = 12000, onOpen } = {}) {
  const toast = () => window.loopxToast;
  const keys = new Set();
  return {
    push({ loop, kind, text }) {
      keys.add(loop);
      toast()(text, { kind, key: loop, ttlMs, max, onOpen });
    },
    /// What is on screen, oldest first: `[{loop, kind, text}]`.
    items: () => toast().items().map((t) => ({ loop: t.key, kind: t.kind, text: t.text })),
    dispose() {
      for (const k of keys) toast().clear(k);
      keys.clear();
    },
  };
}
