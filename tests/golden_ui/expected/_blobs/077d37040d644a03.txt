/* Memory page live updates. While a profile job or a guide build runs, ask /insights/memory/jobs for
   its step every 2s (only while the tab is visible), move the step text and progress
   bar in place, and when the job ends reload the page once, unless something typed
   or an open editor would be lost: then show a "Finished" banner with a Reload link.
   Only textContent is ever written; nothing from the response becomes markup. */
(function () {
  var root = document.querySelector('[data-mem-live]');
  if (!root) return;
  var url = root.getAttribute('data-mem-url') || '/insights/memory/jobs';
  var project = root.getAttribute('data-mem-project') || '';
  var watching = root.hasAttribute('data-mem-watch');
  var els = Array.prototype.slice.call(document.querySelectorAll('[data-mem-job]'));
  // Guide builds have their own strips and their own list in the response.
  var guideEls = Array.prototype.slice.call(document.querySelectorAll('[data-mem-guide-job]'));
  if (!els.length && !guideEls.length && !watching) return;

  var startedAt = Date.now();
  var LIMIT = 2 * 60 * 60 * 1000; // stop after two hours
  var WATCH_WINDOW = 10000; // with nothing running on the page yet, look for 10s
  var delay = 2000;
  var timer = null;
  var done = false;
  var inflight = false;

  function schedule(ms) {
    clearTimeout(timer);
    timer = setTimeout(tick, ms);
  }

  function pageUrl() {
    return url + (project ? '?project=' + encodeURIComponent(project) : '');
  }

  // Typing, an open dialog or an open editor must never be thrown away by a reload.
  function isDirty() {
    if (document.querySelector('dialog[open]')) return true;
    var fields = document.querySelectorAll('textarea, input');
    for (var i = 0; i < fields.length; i++) {
      var f = fields[i];
      var type = (f.getAttribute('type') || 'text').toLowerCase();
      if (type === 'hidden' || type === 'submit' || type === 'button' || type === 'checkbox' || type === 'radio') continue;
      if (f.value !== f.defaultValue) return true;
    }
    var open = document.querySelectorAll('details[open]');
    for (var j = 0; j < open.length; j++) {
      if (open[j].querySelector('textarea, input:not([type="hidden"])')) return true;
    }
    return false;
  }

  function banner() {
    if (document.querySelector('[data-mem-banner]')) return;
    var box = document.createElement('div');
    box.className = 'mem-notice mem-notice-ok mem-live-banner';
    box.setAttribute('role', 'status');
    box.setAttribute('data-mem-banner', '');
    var p = document.createElement('p');
    p.textContent = (root.getAttribute('data-mem-finished') || 'Finished') + ' ';
    var a = document.createElement('a');
    a.setAttribute('href', location.pathname + location.search);
    a.textContent = root.getAttribute('data-mem-reload') || 'Reload';
    p.appendChild(a);
    box.appendChild(p);
    var host = document.querySelector('.mem-content') || document.querySelector('.mem-stack') || root.parentNode;
    host.insertBefore(box, host.firstChild);
  }

  // A job ended: reload once, or, with unsaved work on the page, say so and wait.
  function finish() {
    if (done) return;
    done = true;
    clearTimeout(timer);
    if (isDirty()) {
      banner();
      return;
    }
    // The "Building…" notice in the address would greet the finished page: drop it.
    try {
      var u = new URL(location.href);
      if (/^((build|refresh)(-all)?|guide-build)-started$/.test(u.searchParams.get('notice') || '')) {
        u.searchParams.delete('notice');
        history.replaceState(null, '', u.pathname + u.search + u.hash);
      }
    } catch (e) {}
    location.reload();
  }

  function find(list, alias) {
    for (var i = 0; i < list.length; i++) if (list[i].alias === alias) return list[i];
    return null;
  }

  function show(el, info) {
    var step = el.querySelector('[data-mem-step]');
    if (step && step.textContent !== info.label) step.textContent = info.label;
    var chip = el.querySelector('[data-mem-chip]');
    if (chip) {
      var base = chip.getAttribute('data-mem-base') || '';
      chip.textContent = info.steps > 0 && info.step > 0 ? base + ' ' + info.step + '/' + info.steps : base;
      if (chip.parentNode) chip.parentNode.setAttribute('title', info.label);
    }
    var bar = el.querySelector('[data-mem-progress]');
    if (bar) {
      var pct = info.steps > 0 ? Math.min(100, Math.round((100 * info.step) / info.steps)) : 0;
      bar.setAttribute('aria-valuenow', String(pct));
      bar.setAttribute('aria-label', info.label);
      if (bar.firstElementChild) bar.firstElementChild.style.width = pct + '%';
    }
  }

  function apply(data) {
    var running = data.running || [];
    var ended = false;
    els.forEach(function (el) {
      var key = el.getAttribute('data-mem-job');
      if (key === 'sync') {
        if (data.sync) show(el, data.sync);
        else ended = true;
      } else if (key === 'all') {
        if (!data.build_all) ended = true;
      } else {
        var info = find(running, key);
        if (info) show(el, info);
        else ended = true;
      }
    });
    var guide = data.guide || [];
    guideEls.forEach(function (el) {
      var info = find(guide, el.getAttribute('data-mem-guide-job'));
      if (info) show(el, info);
      else ended = true;
    });
    if (ended) return finish();
    var none = !els.length && !guideEls.length;
    // Nothing on the page shows a job yet, but one has started elsewhere.
    if (none && (running.length || guide.length || data.sync || data.build_all)) return finish();
    if (none && Date.now() - startedAt > WATCH_WINDOW) {
      done = true;
      return;
    }
    delay = 2000;
    schedule(delay);
  }

  function tick() {
    if (done) return;
    if (Date.now() - startedAt > LIMIT) {
      done = true;
      return;
    }
    if (document.hidden) return; // resumed by visibilitychange
    if (inflight) return;
    inflight = true;
    fetch(pageUrl(), { cache: 'no-store', credentials: 'same-origin' })
      .then(function (r) {
        if (!r.ok) throw new Error(String(r.status));
        return r.json();
      })
      .then(function (data) {
        inflight = false;
        apply(data);
      })
      .catch(function () {
        inflight = false;
        delay = Math.min(delay * 2, 15000);
        schedule(delay);
      });
  }

  document.addEventListener('visibilitychange', function () {
    if (done) return;
    if (!document.hidden) {
      clearTimeout(timer);
      tick();
    }
  });

  schedule(600);
})();
