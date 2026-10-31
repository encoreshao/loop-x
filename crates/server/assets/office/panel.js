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
  let el = null, current = null, busy = false, topicMenu = null;

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
    const suggestions = [];
    for (let i = 1; i <= 8; i++) if (t("human_" + i)) suggestions.push(t("human_" + i));
    const combo = topicCombo(suggestions);
    topicMenu = combo;
    const topic = combo.input;
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
    sec.append(h, shops, h2, combo.wrap, chats, msg);
    return sec;
  }

  /// The topic field: free text plus a macOS-style pop-up menu of everyday suggestions
  /// (a WAI-ARIA combobox). The browser's own suggestion list drops down unstyled in WKWebView.
  /// The menu is `position: fixed` so the card's own scrolling never clips it.
  function topicCombo(options) {
    const wrap = document.createElement("div");
    wrap.className = "office-combo";
    const input = document.createElement("input");
    input.type = "text";
    input.maxLength = 60;
    input.className = "office-panel-topic";
    input.placeholder = t("any_topic");
    input.autocomplete = "off";
    input.setAttribute("aria-label", t("talk_about"));
    input.setAttribute("role", "combobox");
    input.setAttribute("aria-autocomplete", "list");
    input.setAttribute("aria-expanded", "false");
    const menuId = "office-topic-menu";
    input.setAttribute("aria-controls", menuId);
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "office-combo-toggle";
    toggle.tabIndex = -1;
    toggle.setAttribute("aria-label", t("talk_about"));
    toggle.innerHTML = "<svg viewBox='0 0 10 16' aria-hidden='true'><path d='M2 6l3-3 3 3M2 10l3 3 3-3' fill='none' stroke='currentColor' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round'/></svg>";
    const menu = document.createElement("ul");
    menu.className = "office-menu";
    menu.id = menuId;
    menu.setAttribute("role", "listbox");
    menu.setAttribute("aria-label", t("talk_about"));
    menu.hidden = true;
    let active = -1, shown = [];

    function place() {
      const r = input.getBoundingClientRect();
      menu.style.left = r.left + "px";
      menu.style.top = r.bottom + 4 + "px";
      menu.style.width = r.width + "px";
    }
    function highlight(i) {
      active = i;
      [...menu.children].forEach((li, j) => li.setAttribute("aria-selected", String(j === i)));
      const li = menu.children[i];
      if (li) {
        input.setAttribute("aria-activedescendant", li.id);
        li.scrollIntoView({ block: "nearest" });
      } else input.removeAttribute("aria-activedescendant");
    }
    function open(filter) {
      const needle = (filter || "").trim().toLowerCase();
      shown = needle ? options.filter((o) => o.toLowerCase().includes(needle)) : options.slice();
      if (!shown.length) return close();
      menu.replaceChildren(...shown.map((text, i) => {
        const li = document.createElement("li");
        li.className = "office-menu-item";
        li.id = menuId + "-" + i;
        li.setAttribute("role", "option");
        li.setAttribute("aria-selected", "false");
        if (text === input.value) li.dataset.checked = "true";
        li.textContent = text;
        // mousedown, not click: keeps the focus in the field.
        li.addEventListener("mousedown", (e) => {
          e.preventDefault();
          pick(text);
        });
        li.addEventListener("mousemove", () => { if (active !== i) highlight(i); });
        return li;
      }));
      if (!wrap.contains(menu)) wrap.append(menu);
      menu.hidden = false;
      input.setAttribute("aria-expanded", "true");
      wrap.classList.add("is-open");
      place();
      highlight(-1);
    }
    function close() {
      menu.hidden = true;
      active = -1;
      input.setAttribute("aria-expanded", "false");
      input.removeAttribute("aria-activedescendant");
      wrap.classList.remove("is-open");
    }
    function pick(text) {
      input.value = text;
      close();
      input.focus();
    }
    input.addEventListener("input", () => open(input.value));
    input.addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        if (menu.hidden) return open("");
        const n = shown.length, step = e.key === "ArrowDown" ? 1 : -1;
        highlight(active < 0 ? (step > 0 ? 0 : n - 1) : (active + step + n) % n);
      } else if (e.key === "Enter" && !menu.hidden && active >= 0) {
        e.preventDefault();
        pick(shown[active]);
      }
    });
    input.addEventListener("blur", () => close());
    toggle.addEventListener("mousedown", (e) => e.preventDefault());
    toggle.addEventListener("click", () => {
      if (menu.hidden) {
        input.focus();
        open("");
      } else close();
    });
    wrap.append(input, toggle);
    return { wrap, input, open, close, get isOpen() { return !menu.hidden; } };
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
    const hadFocus = !!keep && document.activeElement === keep;
    const wasOpen = !!keep && keep.getAttribute("aria-expanded") === "true";
    const fresh = assignSection(who);
    const next = fresh.querySelector(".office-panel-topic");
    if (keep) next.value = keep.value;
    if (old) old.replaceWith(fresh);
    else q(".office-panel-loops").previousElementSibling.before(fresh);
    const list = q(".office-panel-loops");
    list.replaceChildren(...(Array.isArray(who.loops) ? who.loops : []).map(row));
    q(".office-panel-empty").hidden = !!list.children.length;
    if (hadFocus) {
      next.focus();
      if (wasOpen && topicMenu) topicMenu.open("");
    }
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
    topicMenu = null;
    current = null;
    busy = false;
    root.removeEventListener("keydown", onKey, true);
    if (fromInside && hooks.onClose) hooks.onClose();
  }

  function onKey(e) {
    if (e.key === "Escape" && el && topicMenu && topicMenu.isOpen) {
      // The suggestions menu closes first; the card stays.
      e.preventDefault();
      e.stopPropagation();
      topicMenu.close();
      return;
    }
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
        // A fixed-position menu would drift away from its field as the card scrolls.
        el.addEventListener("scroll", () => topicMenu && topicMenu.close());
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
