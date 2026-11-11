(function () {
  // Insights -> Memory -> Graph: pan, zoom, hover, select and filter the
  // server-rendered SVG. Every visible string is already in the page (rendered
  // through the translator); this script only moves and copies it.
  var root = document.querySelector('[data-mg]');
  if (!root) return;
  var svg = root.querySelector('.mg-svg');
  var insp = root.querySelector('.mg-inspector');
  var body = insp.querySelector('.mg-insp-body');
  var list = document.querySelector('.mem-graph-list');
  var vb = svg.viewBox.baseVal;
  var home = { x: vb.x, y: vb.y, w: vb.width, h: vb.height };
  var view = { x: home.x, y: home.y, w: home.w, h: home.h };

  root.querySelectorAll('[data-mg-js]').forEach(function (el) { el.hidden = false; });
  if (list && window.matchMedia('(min-width: 640px)').matches) list.open = false;

  // ---- index the drawing
  var byAlias = {}, byIndex = {}, byEdge = {}, tplNode = {}, kindLabel = {};
  svg.querySelectorAll('.mg-node').forEach(function (n) {
    byAlias[n.dataset.alias] = n;
    byIndex[n.dataset.node] = n;
    n.mgEdges = [];
  });
  svg.querySelectorAll('.mg-edge').forEach(function (e) {
    byEdge[e.dataset.edge] = e;
    e.mgA = byAlias[e.dataset.from];
    e.mgB = byAlias[e.dataset.to];
    if (e.mgA) e.mgA.mgEdges.push(e);
    if (e.mgB) e.mgB.mgEdges.push(e);
    // A wide transparent twin makes a thin line easy to point at.
    var hit = e.cloneNode(false);
    hit.removeAttribute('marker-end');
    hit.setAttribute('class', 'mg-hit mg-k-' + e.dataset.kind);
    hit.mgEdge = e;
    e.parentNode.insertBefore(hit, e);
  });
  root.querySelectorAll('template.mg-tpl-node').forEach(function (t) { tplNode[t.dataset.node] = t; });
  var tplEdge = root.querySelector('template.mg-tpl-edge');
  var chips = root.querySelectorAll('.mg-chip');
  chips.forEach(function (c) { kindLabel[c.dataset.kind] = c.querySelector('.mg-chip-label').textContent; });

  function edgeOf(t) { return t && t.closest ? (t.closest('.mg-hit') || {}).mgEdge || t.closest('.mg-edge') : null; }
  function nodeOf(t) { return t && t.closest ? t.closest('.mg-node') : null; }
  function kindHidden(kind) { return svg.classList.contains('mg-hide-' + kind); }

  // ---- highlight
  var selected = null;
  var returnTo = null; // the node focus goes back to when the inspector closes
  function clearLight() {
    svg.classList.remove('is-focusing');
    svg.querySelectorAll('.is-lit').forEach(function (x) { x.classList.remove('is-lit'); });
  }
  function lightNode(n) {
    clearLight();
    svg.classList.add('is-focusing');
    n.classList.add('is-lit');
    n.mgEdges.forEach(function (e) {
      if (kindHidden(e.dataset.kind)) return;
      e.classList.add('is-lit');
      var other = e.mgA === n ? e.mgB : e.mgA;
      if (other) other.classList.add('is-lit');
    });
  }
  function lightEdge(e) {
    clearLight();
    svg.classList.add('is-focusing');
    e.classList.add('is-lit');
    if (e.mgA) e.mgA.classList.add('is-lit');
    if (e.mgB) e.mgB.classList.add('is-lit');
  }
  function restore() {
    if (!selected) clearLight();
    else if (selected.node) lightNode(selected.node);
    else lightEdge(selected.edge);
  }

  // ---- inspector
  function open(content) {
    body.replaceChildren(content);
    insp.hidden = false;
    requestAnimationFrame(function () { insp.classList.add('is-open'); });
    insp.scrollTop = 0;
    // Keyboard and screen-reader users land in what they opened.
    insp.focus({ preventScroll: true });
  }
  function unselect() {
    if (selected && selected.node) selected.node.classList.remove('is-selected');
    selected = null;
  }
  function selectNode(n) {
    if (!n || !tplNode[n.dataset.node]) return;
    unselect();
    selected = { node: n };
    n.classList.add('is-selected');
    lightNode(n);
    returnTo = n;
    open(tplNode[n.dataset.node].content.cloneNode(true));
    var r = n.querySelector('.mg-core').getBoundingClientRect();
    keepVisible(r.left + r.width / 2, r.top + r.height / 2);
  }
  function selectEdge(e) {
    if (!e || !tplEdge) return;
    unselect();
    selected = { edge: e };
    lightEdge(e);
    var c = tplEdge.content.cloneNode(true);
    var slot = function (name) { return c.querySelector('[data-slot="' + name + '"]'); };
    var kind = slot('kind');
    kind.textContent = kindLabel[e.dataset.kind] || e.dataset.kind;
    kind.classList.add('mg-k-' + e.dataset.kind);
    slot('from').textContent = e.dataset.from;
    slot('to').textContent = e.dataset.to;
    if (e.mgA) slot('from').dataset.goto = e.mgA.dataset.node;
    if (e.mgB) slot('to').dataset.goto = e.mgB.dataset.node;
    slot('dir').textContent = e.hasAttribute('marker-end') ? '→' : '↔';
    slot('ev').textContent = e.dataset.ev;
    slot('conf').textContent = Math.round(parseFloat(e.dataset.conf) * 100) + '%';
    open(c);
    var m = e.getScreenCTM();
    if (m) {
      var mid = e.getPointAtLength(e.getTotalLength() / 2);
      var q = new DOMPoint(mid.x, mid.y).matrixTransform(m);
      keepVisible(q.x, q.y);
    }
  }
  // Slide the view so a point (in screen pixels) is not hidden under the
  // inspector: it moves to the middle of the part of the stage left of it.
  function keepVisible(px, py) {
    var stage = svg.getBoundingClientRect();
    var panel = insp.getBoundingClientRect();
    if (px < panel.left - 24 || py > panel.bottom + 24 || panel.left - stage.left < 120) return;
    var m = svg.getScreenCTM();
    if (!m) return;
    var tx = stage.left + (panel.left - stage.left) / 2;
    view.x += (px - tx) / m.a;
    apply();
  }
  function closeInspector() {
    var back = returnTo;
    returnTo = null;
    insp.classList.remove('is-open');
    insp.hidden = true;
    unselect();
    clearLight();
    if (back) back.focus({ preventScroll: true });
  }
  insp.addEventListener('click', function (ev) {
    if (ev.target.closest('.mg-close')) { closeInspector(); return; }
    var link = ev.target.closest('[data-edge]');
    if (link && byEdge[link.dataset.edge]) { selectEdge(byEdge[link.dataset.edge]); return; }
    var go = ev.target.closest('[data-goto]');
    if (go && byIndex[go.dataset.goto]) {
      var n = byIndex[go.dataset.goto];
      selectNode(n);
      centerOn(n);
    }
  });

  // ---- pan and zoom (the viewBox is the camera)
  function apply() { svg.setAttribute('viewBox', view.x + ' ' + view.y + ' ' + view.w + ' ' + view.h); }
  function toSvg(cx, cy) {
    var m = svg.getScreenCTM();
    if (!m) return { x: view.x + view.w / 2, y: view.y + view.h / 2 };
    return new DOMPoint(cx, cy).matrixTransform(m.inverse());
  }
  function zoomAt(factor, cx, cy) {
    var p = toSvg(cx, cy);
    var w = Math.min(Math.max(view.w * factor, home.w / 8), home.w * 2);
    var f = w / view.w;
    view.x = p.x - (p.x - view.x) * f;
    view.y = p.y - (p.y - view.y) * f;
    view.w = w;
    view.h = view.h * f;
    apply();
  }
  function zoomCenter(factor) {
    var r = svg.getBoundingClientRect();
    zoomAt(factor, r.left + r.width / 2, r.top + r.height / 2);
  }
  function centerOn(n) {
    var t = n.transform.baseVal.consolidate();
    if (!t) return;
    var x = t.matrix.e, y = t.matrix.f;
    if (x < view.x || x > view.x + view.w || y < view.y || y > view.y + view.h) {
      view.x = x - view.w / 2;
      view.y = y - view.h / 2;
      apply();
    }
  }
  root.querySelector('.mg-zoom').addEventListener('click', function (ev) {
    var b = ev.target.closest('[data-zoom]');
    if (!b) return;
    if (b.dataset.zoom === 'in') zoomCenter(1 / 1.3);
    else if (b.dataset.zoom === 'out') zoomCenter(1.3);
    else { view = { x: home.x, y: home.y, w: home.w, h: home.h }; apply(); }
  });
  // Plain scrolling scrolls the page; Ctrl/Cmd + scroll (and a trackpad pinch,
  // which arrives as ctrl + wheel) zooms. A plain scroll shows the hint.
  var stage = root.querySelector('.mg-stage');
  var hint = document.createElement('div');
  hint.className = 'mg-wheel-hint';
  hint.setAttribute('aria-hidden', 'true');
  hint.textContent = stage.dataset.wheelHint || '';
  stage.appendChild(hint);
  var hintTimer = 0;
  svg.addEventListener('wheel', function (ev) {
    if (!(ev.ctrlKey || ev.metaKey)) {
      hint.classList.add('is-on');
      clearTimeout(hintTimer);
      hintTimer = setTimeout(function () { hint.classList.remove('is-on'); }, 1200);
      return;
    }
    hint.classList.remove('is-on');
    ev.preventDefault();
    var k = ev.ctrlKey ? 0.01 : 0.0015;
    zoomAt(Math.exp(ev.deltaY * k), ev.clientX, ev.clientY);
  }, { passive: false });

  var pointers = new Map();
  var drag = null;
  svg.addEventListener('pointerdown', function (ev) {
    if (ev.button !== 0 && ev.pointerType === 'mouse') return;
    pointers.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
    if (pointers.size === 1) {
      drag = { x: ev.clientX, y: ev.clientY, view: { x: view.x, y: view.y }, moved: false, target: ev.target, id: ev.pointerId };
    } else if (pointers.size === 2) {
      var pts = Array.from(pointers.values());
      drag = { pinch: Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y), moved: true };
    }
  });
  svg.addEventListener('pointermove', function (ev) {
    if (!drag || !pointers.has(ev.pointerId)) return;
    pointers.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
    if (drag.pinch) {
      var pts = Array.from(pointers.values());
      if (pts.length < 2) return;
      var d = Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
      if (d > 0) zoomAt(drag.pinch / d, (pts[0].x + pts[1].x) / 2, (pts[0].y + pts[1].y) / 2);
      drag.pinch = d;
      return;
    }
    var dx = ev.clientX - drag.x, dy = ev.clientY - drag.y;
    if (!drag.moved && Math.hypot(dx, dy) < 4) return;
    if (!drag.moved) {
      drag.moved = true;
      svg.setPointerCapture(ev.pointerId);
      svg.classList.add('is-panning');
    }
    var m = svg.getScreenCTM();
    var s = m ? m.a : 1;
    view.x = drag.view.x - dx / s;
    view.y = drag.view.y - dy / s;
    apply();
  });
  function end(ev) {
    pointers.delete(ev.pointerId);
    if (!drag) return;
    var d = drag;
    if (pointers.size === 0) { drag = null; svg.classList.remove('is-panning'); }
    if (ev.type !== 'pointerup' || d.moved || d.pinch) return;
    var n = nodeOf(d.target);
    var e = n ? null : edgeOf(d.target);
    if (n) selectNode(n);
    else if (e) selectEdge(e);
    else if (selected) closeInspector();
  }
  svg.addEventListener('pointerup', end);
  svg.addEventListener('pointercancel', end);

  // ---- hover
  svg.addEventListener('pointerover', function (ev) {
    if (drag && drag.moved) return;
    var n = nodeOf(ev.target);
    if (n) { lightNode(n); return; }
    var e = edgeOf(ev.target);
    if (e) lightEdge(e);
  });
  svg.addEventListener('pointerout', function (ev) {
    if (drag && drag.moved) return;
    var to = ev.relatedTarget;
    if (nodeOf(to) || edgeOf(to)) return;
    restore();
  });

  // ---- keyboard
  svg.addEventListener('keydown', function (ev) {
    var n = nodeOf(ev.target);
    if (n && (ev.key === 'Enter' || ev.key === ' ')) {
      ev.preventDefault();
      selectNode(n);
    }
  });
  svg.addEventListener('focusin', function (ev) { var n = nodeOf(ev.target); if (n) lightNode(n); });
  svg.addEventListener('focusout', function () { restore(); });
  document.addEventListener('keydown', function (ev) {
    if (ev.key === 'Escape' && !insp.hidden) closeInspector();
  });

  // ---- link-kind filters
  chips.forEach(function (c) {
    c.addEventListener('click', function () {
      var on = c.getAttribute('aria-pressed') !== 'true';
      c.setAttribute('aria-pressed', on ? 'true' : 'false');
      [svg, insp, list].forEach(function (el) {
        if (el) el.classList.toggle('mg-hide-' + c.dataset.kind, !on);
      });
      if (!on && selected && selected.edge && selected.edge.dataset.kind === c.dataset.kind) closeInspector();
      else restore();
    });
  });
})();
