// The details panel of the Office view: a keyboard-reachable HTML overlay inside
// #office, cloned from the server-rendered `<template id="office-panel">`. Every
// piece of loop data goes in through `textContent` or `encodeURIComponent`d URLs,
// never as markup. Actions are same-origin POSTs carrying the page's CSRF token;
// the routes answer with a redirect and a flash that `redirect: "manual"` hides,
// so the panel just refetches the state (`hooks.refresh`) and shows a generic
// "Action failed" only when the response itself was an error.

const enc = encodeURIComponent;

/// `createPanel(root, i18n, csrfToken, hooks)`: `hooks.refresh()` re-reads the state
/// after an action (and calls `update`), `hooks.onClose()` runs when the panel is
/// dismissed from inside (Escape, the close button, a loop that disappeared).
export function createPanel(root, i18n, csrfToken, hooks = {}) {
  const template = document.getElementById("office-panel");
  let el = null, current = null, busy = false;

  const t = (k) => i18n[k] || "";
  const q = (sel) => el.querySelector(sel);

  function when(iso) {
    if (!iso) return "—";
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return String(iso);
    // The dashboard's language (<html lang>), not the browser's.
    try {
      return d.toLocaleString(document.documentElement.lang || undefined);
    } catch {
      return d.toLocaleString();
    }
  }

  function render(loop) {
    current = loop;
    const name = String(loop.name);
    const state = String(loop.state);
    q(".office-panel-name").textContent = loop.title ? String(loop.title) : name;
    const chip = q(".office-panel-state");
    chip.textContent = t("state_" + state) || state;
    chip.dataset.state = state;
    q(".office-panel-why").textContent = loop.reason || "—";
    q(".office-panel-last").textContent = when(loop.last_run);
    q(".office-panel-next").textContent = loop.next_run || "—";

    const history = q(".office-panel-history");
    history.replaceChildren();
    for (const h of Array.isArray(loop.history) ? loop.history : []) {
      const li = document.createElement("li");
      const a = document.createElement("a");
      a.href = "/loops/" + enc(name) + "/history/" + enc(String(h.file));
      a.textContent = String(h.name || h.file);
      li.append(a);
      history.append(li);
    }
    const historyBlock = q(".office-panel-history-block");
    historyBlock.hidden = !history.children.length;

    const approvals = q(".office-panel-approvals");
    const n = Number(loop.pending_approvals) || 0;
    approvals.hidden = !(state === "waiting" && n > 0);
    const link = q("[data-act=approvals]");
    link.href = "/?view=activity";
    link.textContent = t("pending_approvals") + ": " + n;

    q("[data-act=open]").href = "/loops/" + enc(name);
    const run = q("[data-act=run]");
    const runOff = state === "working" || state === "disabled";
    run.setAttribute("aria-disabled", String(runOff));
    run.classList.toggle("is-disabled", runOff);
    q("[data-act=toggle]").textContent = loop.enabled ? t("disable") : t("enable");
  }

  async function act(kind) {
    if (!current || busy) return;
    const url = kind === "run" ? current.run_now_url : current.toggle_url;
    if (!url) return;
    if (kind === "run" && el.querySelector("[data-act=run]").getAttribute("aria-disabled") === "true") return;
    busy = true;
    el.setAttribute("aria-busy", "true");
    q(".office-panel-error").hidden = true;
    // The routes answer with a 303 to `...?flash=<message>&ok=0|1`; the fetch follows it
    // and only the final URL is read (the page behind it is never parsed).
    let failed = null;
    try {
      const r = await fetch(url, {
        method: "POST",
        body: new URLSearchParams({ csrf_token: csrfToken }),
        credentials: "same-origin",
      });
      if (r.status === 403) failed = t("reload") || t("action_failed");
      else if (!r.ok) failed = t("action_failed");
      else {
        let params = null;
        try {
          params = new URL(r.url).searchParams;
        } catch {
          /* a response without a URL carries no flash */
        }
        if (params && params.get("ok") === "0") failed = params.get("flash") || t("action_failed");
      }
    } catch {
      failed = t("action_failed");
    }
    if (kind === "run" && !failed && hooks.onRun) hooks.onRun(current ? current.name : null);
    try {
      if (hooks.refresh) await hooks.refresh();
    } catch {
      /* the next refresh will catch up */
    }
    busy = false;
    if (el) {
      el.removeAttribute("aria-busy");
      const err = q(".office-panel-error");
      if (failed) err.textContent = failed;
      err.hidden = !failed;
    }
  }

  function close(fromInside = true) {
    if (!el) return;
    el.remove();
    el = null;
    current = null;
    busy = false;
    root.removeEventListener("keydown", onKey, true);
    if (fromInside && hooks.onClose) hooks.onClose();
  }

  function onKey(e) {
    if (e.key === "Escape" && el) {
      e.preventDefault();
      close();
    }
  }

  return {
    open(loop) {
      if (!template || !loop) return;
      if (!el) {
        el = template.content.firstElementChild.cloneNode(true);
        q(".office-panel-close").addEventListener("click", () => close());
        for (const b of el.querySelectorAll("[data-act=run], [data-act=toggle]")) {
          b.addEventListener("click", () => act(b.dataset.act));
        }
        // aria-disabled (not disabled) keeps the button focusable and announced.
        el.addEventListener("click", (e) => {
          if (e.target.closest("[data-act=run][aria-disabled=true]")) e.preventDefault();
        });
        root.append(el);
        // Scoped to the office: focus is inside it whenever the panel was opened.
        root.addEventListener("keydown", onKey, true);
        q(".office-panel-close").focus();
      }
      render(loop);
    },
    close() {
      close(false);
    },
    /// Fresh states: re-draws the open loop, or closes if it is gone.
    update(loops) {
      if (!el || !current) return;
      const next = (Array.isArray(loops) ? loops : []).find((l) => l && l.name === current.name);
      if (next) render(next);
      else close();
    },
    get isOpen() {
      return !!el;
    },
    get name() {
      return current ? current.name : null;
    },
  };
}
