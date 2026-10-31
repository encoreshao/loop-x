// The bottom-left notification stack of the Office view. Newest at the bottom; at most `max`
// shown, the oldest leaving first. A loop has one card for its current run: a step update or
// the run's outcome replaces that card rather than stacking another. Started, step and done
// cards fade away after `ttlMs` (paused while hovered or focused); failures and approvals
// stay until dismissed. Colours come from the dashboard's theme tokens (see office.rs).

const STICKY = new Set(["failed", "waiting"]);

/// `createToasts(root, strings, {max, ttlMs, onOpen})` returns `{push, items, dispose}`.
export function createToasts(root, strings = {}, { max = 10, ttlMs = 12000, onOpen } = {}) {
  const list = document.createElement("ol");
  list.className = "office-toasts";
  list.setAttribute("role", "log");
  list.setAttribute("aria-live", "polite");
  list.setAttribute("aria-label", strings.toast_region || "");
  root.append(list);
  const timers = new Map();
  const live = () => [...list.children].filter((li) => !li.classList.contains("leaving"));

  function leave(li) {
    if (li.classList.contains("leaving")) return;
    clearTimeout(timers.get(li));
    timers.delete(li);
    li.classList.add("leaving");
    const gone = () => li.remove();
    li.addEventListener("animationend", gone, { once: true });
    setTimeout(gone, 400);
  }

  function arm(li) {
    clearTimeout(timers.get(li));
    if (STICKY.has(li.dataset.kind)) return;
    timers.set(li, setTimeout(() => leave(li), ttlMs));
  }

  function push({ loop, kind, text }) {
    // A loop shows one card: the new one replaces the earlier, except that an unread failure
    // stays up when the next run starts, until that run's own outcome replaces it.
    for (const li of live()) {
      if (li.dataset.loop === loop && !(li.dataset.kind === "failed" && (kind === "started" || kind === "step"))) {
        clearTimeout(timers.get(li));
        timers.delete(li);
        li.remove();
      }
    }
    const li = document.createElement("li");
    li.className = "office-toast";
    li.dataset.loop = loop;
    li.dataset.kind = kind;
    li.tabIndex = 0;
    const dot = document.createElement("span");
    dot.className = "office-toast-dot";
    dot.setAttribute("aria-hidden", "true");
    const body = document.createElement("span");
    body.className = "office-toast-body";
    const title = document.createElement("span");
    title.className = "office-toast-title";
    title.textContent = text;
    const time = document.createElement("time");
    time.className = "office-toast-time";
    const now = new Date();
    time.dateTime = now.toISOString();
    time.textContent = now.toLocaleTimeString(document.documentElement.lang || undefined, { hour: "2-digit", minute: "2-digit" });
    body.append(title, time);
    const close = document.createElement("button");
    close.type = "button";
    close.className = "office-toast-close";
    close.setAttribute("aria-label", strings.toast_dismiss || "");
    close.title = strings.toast_dismiss || "";
    close.textContent = "×";
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      leave(li);
    });
    li.append(dot, body, close);
    li.addEventListener("click", () => onOpen && onOpen(loop));
    li.addEventListener("keydown", (e) => {
      if (e.target !== li) return;
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        if (onOpen) onOpen(loop);
      } else if (e.key === "Escape" || e.key === "Delete" || e.key === "Backspace") {
        e.preventDefault();
        leave(li);
      }
    });
    // Hovering or focusing a card holds it; leaving it starts its full time again.
    li.addEventListener("pointerenter", () => clearTimeout(timers.get(li)));
    li.addEventListener("pointerleave", () => arm(li));
    li.addEventListener("focusin", () => clearTimeout(timers.get(li)));
    li.addEventListener("focusout", (e) => li.contains(e.relatedTarget) || arm(li));
    list.append(li);
    arm(li);
    const shown = live();
    for (let i = 0; i < shown.length - max; i++) leave(shown[i]);
  }

  return {
    push,
    /// What is on screen, oldest first: `[{loop, kind, text}]`.
    items: () => live().map((li) => ({ loop: li.dataset.loop, kind: li.dataset.kind, text: li.querySelector(".office-toast-title").textContent })),
    dispose() {
      for (const t of timers.values()) clearTimeout(t);
      timers.clear();
      list.remove();
    },
  };
}
