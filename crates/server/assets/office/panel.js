// The details card of a colleague in the Office view: their state and loops, each with
// Run now and Enable/Disable. A keyboard-reachable HTML overlay inside
// #office, cloned from the server-rendered `<template id="office-panel">`. Every
// piece of loop data goes in through `textContent` or `encodeURIComponent`d URLs,
// never as markup. Actions are same-origin POSTs carrying the page's CSRF token;
// the routes answer with a redirect and a flash that `redirect: "manual"` hides,
// so the panel just refetches the state (`hooks.refresh`) and shows a generic
// "Action failed" only when the response itself was an error.

const enc = encodeURIComponent;

/// `createPanel(root, i18n, csrfToken, hooks)`: `hooks.refresh()` re-reads the state
/// after an action (and calls `update`), `hooks.onRun(loopName)` runs after a started
/// run, `hooks.onClose()` runs when the panel is dismissed from inside (Escape, the
/// close button).
export function createPanel(root, i18n, csrfToken, hooks = {}) {
  const template = document.getElementById("office-panel");
  let el = null, current = null, busy = false;

  const t = (k) => i18n[k] || "";
  const q = (sel) => el.querySelector(sel);

  /// One row per loop of the colleague: its name (a link), state, Run now and Enable/Disable.
  function row(loop) {
    const name = String(loop.name), state = String(loop.state);
    const li = document.createElement("li");
    li.className = "office-panel-loop";
    li.dataset.loop = name;
    const a = document.createElement("a");
    a.href = "/loops/" + enc(name);
    a.textContent = loop.title ? String(loop.title) : name;
    const chip = document.createElement("span");
    chip.className = "office-panel-state";
    chip.dataset.state = state;
    chip.textContent = t("state_" + state) || state;
    const run = document.createElement("button");
    run.type = "button";
    run.dataset.act = "run";
    run.textContent = t("run_now");
    // aria-disabled (not disabled) keeps the button focusable and announced.
    const runOff = state === "working" || state === "disabled";
    run.setAttribute("aria-disabled", String(runOff));
    run.classList.toggle("is-disabled", runOff);
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.dataset.act = "toggle";
    toggle.textContent = loop.enabled ? t("disable") : t("enable");
    const actions = document.createElement("div");
    actions.className = "office-panel-actions";
    actions.append(run, toggle);
    for (const b of [run, toggle]) b.addEventListener("click", () => act(b.dataset.act, loop, b));
    li.append(a, chip, actions);
    return li;
  }

  /// The "Send out" and "Talk" controls: shops, calling back, and a conversation with a colleague
  /// about a topic (typed, or one of the everyday suggestions).
  function assignSection(who) {
    const sec = document.createElement("div");
    sec.className = "office-panel-assign";
    const btn = (text, onClick, off) => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = text;
      b.setAttribute("aria-disabled", String(!!off));
      b.addEventListener("click", () => {
        if (off) return say(t("unavailable").replace("{name}", String(who.title || who.name)));
        const ok = onClick();
        if (!ok) say(t("unavailable").replace("{name}", String(who.title || who.name)));
        else if (hooks.people) {
          const next = hooks.people().find((p) => p.name === who.name);
          if (next) render(next);
        }
      });
      return b;
    };
    const h = document.createElement("h3");
    h.textContent = t("send_out");
    const shops = document.createElement("div");
    shops.className = "office-panel-row";
    shops.dataset.row = "shops";
    if (who.away) shops.append(btn(t("call_back"), () => hooks.assign(who.name, "back")));
    else {
      shops.append(btn(t("coffee_shop"), () => hooks.assign(who.name, "coffee"), !who.free));
      shops.append(btn(t("bookstore"), () => hooks.assign(who.name, "books"), !who.free));
    }
    const h2 = document.createElement("h3");
    h2.textContent = t("talk_about");
    const topic = document.createElement("input");
    topic.type = "text";
    topic.maxLength = 60;
    topic.className = "office-panel-topic";
    topic.placeholder = t("any_topic");
    topic.setAttribute("aria-label", t("talk_about"));
    topic.setAttribute("list", "office-topics");
    const list = document.createElement("datalist");
    list.id = "office-topics";
    for (let i = 1; i <= 8; i++) {
      const o = document.createElement("option");
      o.value = t("human_" + i);
      list.append(o);
    }
    const chats = document.createElement("div");
    chats.className = "office-panel-row";
    chats.dataset.row = "chats";
    const others = hooks.people ? hooks.people().filter((p) => p.name !== who.name) : [];
    for (const o of others) {
      chats.append(btn(t("chat_with").replace("{name}", String(o.title || o.name)), () => hooks.assign(who.name, "talk", { with: o.name, topic: topic.value }), !who.canTalk || !o.canTalk));
    }
    const msg = document.createElement("p");
    msg.className = "office-panel-assign-msg";
    msg.setAttribute("role", "status");
    msg.hidden = true;
    function say(text) {
      msg.textContent = text;
      msg.hidden = !text;
    }
    sec.append(h, shops, h2, topic, list, chats, msg);
    return sec;
  }

  function render(who) {
    current = who;
    q(".office-panel-name").textContent = String(who.title || who.name);
    const state = String(who.state);
    const chip = q(".office-panel-state");
    chip.textContent = t("state_" + state) || state;
    chip.dataset.state = state;
    const note = q(".office-panel-note");
    note.textContent = who.note ? t(who.note) : "";
    note.hidden = !who.note;
    const old = q(".office-panel-assign");
    const keep = old ? old.querySelector(".office-panel-topic") : null;
    const fresh = assignSection(who);
    if (keep) fresh.querySelector(".office-panel-topic").value = keep.value;
    if (old) old.replaceWith(fresh);
    else q(".office-panel-loops").previousElementSibling.before(fresh);
    const list = q(".office-panel-loops");
    list.replaceChildren(...(Array.isArray(who.loops) ? who.loops : []).map(row));
    q(".office-panel-empty").hidden = !!list.children.length;
  }

  async function act(kind, loop, button) {
    if (!current || busy) return;
    const url = kind === "run" ? loop.run_now_url : loop.toggle_url;
    if (!url) return;
    if (kind === "run" && button.getAttribute("aria-disabled") === "true") return;
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
    if (kind === "run" && !failed && hooks.onRun) hooks.onRun(loop.name);
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
    /// Fresh states of the colleagues: re-draws the open card, or closes if it is gone.
    update(people) {
      if (!el || !current) return;
      const next = (Array.isArray(people) ? people : []).find((p) => p && p.name === current.name);
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
