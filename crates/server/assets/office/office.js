// Entry module for the Office view: checks WebGL (falling back to the List view
// with a note), mounts the renderer in #office, builds the world, runs day/night
// and the render loop, and seats one person per loop from `/office/state`. Test
// hooks in the URL hash: `h=<hour>` overrides the clock, `left`/`right`/`in` pick a
// camera (e.g. `#left&h=23`). Hovering a person highlights their name tag; clicking
// one (or `select(name)`) opens the details panel and eases the camera to them,
// clicking empty space or pressing Escape releases it. `window.__office` exposes
// `debug()` (where everyone is), `setLoops(loops)` (draw these states instead),
// `overflow`, `syncs`, and the test hooks `select(name|null)`, `selected()`,
// `hover()`, `panelOpen()`, `screenOf(name)`, `focus()` and `dispose()`. The state
// stays live (live.js); `#stale=<ms>` and `#poll=<ms>` in the hash shorten its timers
// for tests.
import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";

// Sibling modules get this module's ?v= cache-buster too.
const V = new URL(import.meta.url).search;
const root = document.getElementById("office");

function readStrings() {
  try {
    return JSON.parse(document.getElementById("office-i18n").textContent) || {};
  } catch {
    return {};
  }
}
const strings = readStrings();

function webglAvailable() {
  try {
    const gl = document.createElement("canvas").getContext("webgl2") || document.createElement("canvas").getContext("webgl");
    // Release the probe at once: browsers cap the number of live contexts.
    if (gl) gl.getExtension("WEBGL_lose_context")?.loseContext();
    return !!gl;
  } catch {
    return false;
  }
}

/// Hides the office and shows the server-rendered Dashboard link with a one-line note. The
/// `loop_mode` cookie is left alone, so a browser with WebGL still gets the office.
function showList(key = "no_webgl") {
  if (root) root.hidden = true;
  const list = document.getElementById("office-list");
  if (!list) return;
  if (!list.querySelector(".office-note")) {
    const note = document.createElement("p");
    note.className = "office-note";
    note.setAttribute("role", "status");
    note.textContent = strings[key] || "";
    list.prepend(note);
  }
  list.hidden = false;
}

/// `{camera, hour}` from the URL hash, e.g. `#left&h=23`.
function hashHooks() {
  const parts = location.hash.replace(/^#/, "").split("&");
  const camera = ["left", "right", "in"].find((c) => parts.includes(c)) || null;
  const h = parts.map((p) => p.match(/^h=([\d.]+)$/)).find(Boolean);
  const num = (key) => {
    const m = parts.map((p) => p.match(new RegExp("^" + key + "=(\\d+)$"))).find(Boolean);
    return m ? Number(m[1]) : null;
  };
  // Floors keep a stray hash from hammering the server.
  const floor = (v, min) => (v === null ? null : Math.max(min, v));
  return { camera, hour: h ? Number(h[1]) : null, staleMs: floor(num("stale"), 1000), pollMs: floor(num("poll"), 250) };
}

function timeControl(onPick, initial) {
  const presets = [["auto", null], ["day", 13], ["dusk", 17.9], ["night", 23]];
  const group = document.createElement("div");
  group.className = "office-tod";
  group.setAttribute("role", "group");
  group.setAttribute("aria-label", strings.time_of_day || "");
  const buttons = presets.map(([key, hour]) => {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = strings[key] || key;
    b.setAttribute("aria-pressed", String(hour === initial));
    b.addEventListener("click", () => {
      for (const o of buttons) o.setAttribute("aria-pressed", String(o === b));
      onPick(hour);
    });
    group.append(b);
    return b;
  });
  return group;
}

/// An AbortSignal that fires after `ms` (older browsers lack `AbortSignal.timeout`).
function timeoutSignal(ms) {
  if (typeof AbortSignal !== "undefined" && AbortSignal.timeout) return AbortSignal.timeout(ms);
  const c = new AbortController();
  setTimeout(() => c.abort(), ms);
  return c.signal;
}

function disposeScene(scene) {
  scene.traverse((o) => {
    if (o.geometry) o.geometry.dispose();
    const mats = Array.isArray(o.material) ? o.material : o.material ? [o.material] : [];
    for (const m of mats) {
      for (const v of Object.values(m)) if (v && v.isTexture) v.dispose();
      m.dispose();
    }
  });
}

async function main() {
  if (!root) return;
  if (!webglAvailable()) {
    showList();
    return;
  }
  const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const [{ buildWorld, buildLights, freezeStatic }, { createDayNight }, { createAgents }, { createPanel }, { startLive }] = await Promise.all([
    import("./world.js" + V),
    import("./daynight.js" + V),
    import("./agents.js" + V),
    import("./panel.js" + V),
    import("./live.js" + V),
  ]);

  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({ antialias: true });
  } catch {
    showList();
    return;
  }
  const size = () => [Math.max(1, root.clientWidth), Math.max(1, root.clientHeight)];
  const [w0, h0] = size();
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  renderer.setSize(w0, h0);
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = THREE.PCFSoftShadowMap;
  const canvas = renderer.domElement;
  canvas.setAttribute("role", "img");
  canvas.setAttribute("aria-label", strings.canvas_label || "");
  root.prepend(canvas);

  const scene = new THREE.Scene();
  scene.background = new THREE.Color(0x20232a);
  const camera = new THREE.PerspectiveCamera(42, w0 / h0, 0.1, 250);
  camera.position.set(2, 22, 32);
  const controls = new OrbitControls(camera, canvas);
  controls.target.set(0, 1, 1);
  controls.enableDamping = true;
  controls.maxPolarAngle = Math.PI / 2.05;
  const hooks = hashHooks();
  if (hooks.camera === "left") {
    camera.position.set(-40, 9, 1);
    controls.target.set(-15, 3.5, 1);
  } else if (hooks.camera === "right") {
    camera.position.set(40, 9, 1);
    controls.target.set(15, 3.5, 1);
  } else if (hooks.camera === "in") {
    camera.position.set(0, 7, 17);
    controls.target.set(0, 3.5, -2);
  }
  controls.update();

  const lights = buildLights(scene, THREE);
  const { handles } = buildWorld(scene, THREE, { strings, reducedMotion });
  // Only the clouds, cars, birds, walkers, sun and moon move; the rest keeps its matrices.
  const movers = [
    ...handles.clouds, ...handles.cars.map((c) => c.g), ...handles.birds.map((b) => b.g), ...handles.walkers.map((w) => w.g),
    handles.sun.mesh, handles.sun.halo, handles.moon.mesh, handles.moon.halo,
  ];
  freezeStatic(scene, movers);
  const worldObjects = new Set(scene.children);
  const dayNight = createDayNight(THREE, scene, handles, lights);
  const agents = createAgents({ THREE, scene, handles, strings, reducedMotion });
  // "and N more" for loops beyond the ones drawn; the List view has them all.
  const more = document.createElement("p");
  more.className = "office-more";
  more.hidden = true;
  root.append(more);
  // Shown while the server cannot be reached: what is drawn is the last state we got.
  const staleNote = document.createElement("p");
  staleNote.className = "office-stale";
  staleNote.setAttribute("role", "status");
  staleNote.textContent = strings.stale || "";
  staleNote.hidden = true;
  root.append(staleNote);
  const office = (window.__office = Object.assign(window.__office || {}, { overflow: 0, syncs: 0 }));
  let frame = 0, onScreen = true, disposed = false, lastFrame = 0, boostUntil = 0, wanted = 2;
  const clock = new THREE.Clock();
  let latest = [], selected = null, hovered = null, focusAnim = null;
  const csrf = () => (document.getElementById("office-csrf") || {}).value || "";
  root.tabIndex = -1;
  const panel = createPanel(root, strings, csrf(), {
    refresh: () => refresh(),
    onRun: (name) => name && agents.runRequested(name),
    onClose: () => release(),
  });
  function show(loops) {
    latest = Array.isArray(loops) ? loops : [];
    agents.sync(latest);
    panel.update(latest);
    if (selected && !panel.isOpen) release();
    const n = agents.overflow;
    more.textContent = n ? (strings.and_more || "").replace("{n}", String(n)) : "";
    more.hidden = !n;
    office.overflow = n;
    office.syncs += 1;
    requestRender();
  }
  async function fetchState() {
    const r = await fetch("/office/state", {
      credentials: "same-origin",
      headers: { Accept: "application/json" },
      signal: timeoutSignal(10000),
    });
    if (!r.ok) throw new Error("HTTP " + r.status);
    const loops = (await r.json()).loops;
    // A 200 that is not a state feed counts as a failure: keep what is drawn.
    if (!Array.isArray(loops)) throw new Error("bad state feed");
    return loops;
  }
  // Unchanged states are not redrawn: no panel flicker, nothing moves.
  let lastJson = null;
  function onState(loops) {
    if (disposed) return;
    const json = JSON.stringify(loops);
    if (json === lastJson) return;
    lastJson = json;
    show(loops);
  }
  /// After a panel action: refetch now, even if the state looks unchanged.
  const refresh = () => live.refresh();
  const live = startLive({
    fetchState,
    onState,
    onStale: () => (staleNote.hidden = false),
    onFresh: () => (staleNote.hidden = true),
    pollMs: hooks.pollMs || 30000,
    staleMs: hooks.staleMs || 60000,
  });
  // The first state must not wait for the stream or the poll.
  live.refresh();
  // A test override sticks until the server's own state next changes.
  office.setLoops = show;
  office.runRequested = (name) => agents.runRequested(name);
  // A chat tool call sends the matching employee to their desk, slowly.
  const TOOL_LOOP = { gitlab: "gitlab-loop", mail: "inbox-triage-loop", calendar: "inbox-triage-loop", rss: "topic-loop", web: "topic-loop" };
  const onTool = (ev) => {
    const prefix = String((ev.detail && ev.detail.name) || "").split(".")[0];
    if (TOOL_LOOP[prefix]) agents.toolRequested(TOOL_LOOP[prefix]);
  };
  document.addEventListener("loopx:tool", onTool);
  office.toolRequested = (name) => agents.toolRequested(name);
  office.debug = () => agents.debug();

  // ---------- selection, hover and the camera ----------
  const FOCUS_MS = 600, FOCUS_DIST = 11, CHEST = 1.5;
  const ray = new THREE.Raycaster();
  const ndc = new THREE.Vector2();
  const isShown = (o) => {
    for (; o; o = o.parent) if (!o.visible) return false;
    return true;
  };
  /// The person under the pointer, or null. Furniture in front hides people behind it.
  function pick(ev) {
    const r = canvas.getBoundingClientRect();
    ndc.set(((ev.clientX - r.left) / r.width) * 2 - 1, -((ev.clientY - r.top) / r.height) * 2 + 1);
    ray.setFromCamera(ndc, camera);
    const mine = ray.intersectObjects(agents.pickables(), true).find((h) => !h.object.isLine && isShown(h.object));
    if (!mine) return null;
    const a = agents.agentAt(mine.object);
    for (const h of ray.intersectObjects(occluders(), true)) {
      if (h.distance >= mine.distance - 0.05) break;
      const o = h.object;
      if (o.isLine || o.isPoints || o.isSprite || !isShown(o) || agents.agentAt(o)) continue;
      const m = Array.isArray(o.material) ? o.material[0] : o.material;
      if (m && m.transparent && m.opacity < 0.6) continue;
      return null;
    }
    return a ? a.name : null;
  }
  function highlight() {
    agents.setHighlight([hovered, selected].filter(Boolean));
    requestRender();
  }
  // What can hide a person from the pointer: the room's static objects and furniture,
  // not the sky, the ground or the city outside.
  const ROOM_BOX = new THREE.Box3(new THREE.Vector3(-16, -1, -12), new THREE.Vector3(16, 12, 14));
  let indoor = null, indoorList = [], indoorFurniture = null;
  function occluders() {
    if (!indoor) {
      const box = new THREE.Box3(), size = new THREE.Vector3();
      indoor = [...worldObjects].filter((o) => {
        if (o.isLight || o.isSprite || o.isPoints || o.isLine) return false;
        box.setFromObject(o);
        box.getSize(size);
        return box.intersectsBox(ROOM_BOX) && Math.max(size.x, size.y, size.z) < 60;
      });
    }
    const f = agents.furnitureGroup();
    if (f !== indoorFurniture) {
      indoorFurniture = f;
      indoorList = f ? [...indoor, f] : indoor;
    }
    return indoorList;
  }
  const chestOf = (name) => {
    const p = agents.positionOf(name);
    return p && new THREE.Vector3(p[0], p[1] + CHEST, p[2]);
  };
  function startFocus(name) {
    const to = chestOf(name);
    if (!to) return;
    const dir = camera.position.clone().sub(controls.target);
    dir.y = Math.max(dir.y, dir.length() * 0.35);
    dir.setLength(FOCUS_DIST);
    focusAnim = {
      name, t0: clock.elapsedTime, fromT: controls.target.clone(), fromP: camera.position.clone(),
      dir, last: to.clone(), done: false,
    };
    if (reducedMotion) stepFocus(1e9);
  }
  /// One frame of the ease; afterwards the camera just keeps pace with a walking person.
  function stepFocus(now) {
    const f = focusAnim;
    if (!f) return;
    const to = chestOf(f.name);
    if (!to) {
      focusAnim = null;
      return;
    }
    if (f.done) {
      const d = to.clone().sub(f.last);
      controls.target.add(d);
      camera.position.add(d);
    } else {
      const u = Math.min(1, ((now - f.t0) * 1000) / FOCUS_MS);
      const e = u < 0.5 ? 2 * u * u : 1 - Math.pow(-2 * u + 2, 2) / 2;
      controls.target.lerpVectors(f.fromT, to, e);
      camera.position.lerpVectors(f.fromP, to.clone().add(f.dir), e);
      f.done = u >= 1;
    }
    f.last.copy(to);
  }
  controls.addEventListener("start", () => {
    // Grabbing the camera ends the ease; it still follows the person afterwards.
    if (focusAnim && !focusAnim.done) focusAnim.done = true;
  });
  function select(name) {
    const loop = name == null ? null : latest.find((l) => l && l.name === name);
    if (!loop || !agents.positionOf(loop.name)) {
      release();
      return;
    }
    selected = loop.name;
    panel.open(loop);
    highlight();
    startFocus(loop.name);
    seedChat(loop);
  }
  /// Starts the chat's message about the picked loop, unless the person already typed.
  function seedChat(loop) {
    const box = document.querySelector("#ai-panel-form textarea[name='text']");
    if (!box || !strings.chat_about) return;
    const last = box.dataset.seeded;
    if (box.value && box.value !== last) return;
    box.value = strings.chat_about.replace("{name}", loop.title ? String(loop.title) : String(loop.name));
    box.dataset.seeded = box.value;
    box.dispatchEvent(new Event("input", { bubbles: true }));
  }
  // The chat changed something (a loop ran, was switched, an approval went through).
  document.addEventListener("loopx:changed", refresh);
  document.querySelector("[data-office-chat-toggle]")?.addEventListener("click", () => document.getElementById("ai-panel-trigger")?.click());
  /// Drops the selection: closes the panel, stops following and returns focus to the canvas container.
  function release() {
    const had = selected !== null;
    selected = null;
    focusAnim = null;
    panel.close();
    highlight();
    if (had) root.focus({ preventScroll: true });
  }
  let down = null, lastMove = 0;
  canvas.addEventListener("pointerdown", (e) => {
    down = e.button === 0 ? { x: e.clientX, y: e.clientY } : null;
  });
  canvas.addEventListener("pointerup", (e) => {
    const d = down;
    down = null;
    if (!d || Math.hypot(e.clientX - d.x, e.clientY - d.y) > 5) return;
    const name = pick(e);
    if (name) select(name);
    else release();
  });
  canvas.addEventListener("pointermove", (e) => {
    if (e.buttons) return;
    const now = performance.now();
    if (now - lastMove < 50) return;
    lastMove = now;
    const name = pick(e);
    if (name !== hovered) {
      hovered = name;
      canvas.style.cursor = name ? "pointer" : "";
      highlight();
    }
  });
  canvas.addEventListener("pointerleave", () => {
    if (hovered) {
      hovered = null;
      canvas.style.cursor = "";
      highlight();
    }
  });
  office.select = select;
  office.selected = () => selected;
  office.hover = () => hovered;
  office.panelOpen = () => panel.isOpen;
  office.screenOf = (name) => {
    const p = agents.positionOf(name);
    if (!p) return null;
    const v = new THREE.Vector3(p[0], p[1] + 1.2, p[2]).project(camera);
    const r = canvas.getBoundingClientRect();
    return { x: r.left + ((v.x + 1) / 2) * r.width, y: r.top + ((1 - v.y) / 2) * r.height };
  };
  office.focus = () => {
    const p = selected && chestOf(selected);
    return p ? { target: controls.target.toArray(), person: p.toArray() } : null;
  };
  let override = hooks.hour;
  const apply = () => {
    dayNight.apply(dayNight.currentHour(override));
    requestRender();
  };
  apply();
  const timer = setInterval(apply, 30000);
  root.append(timeControl((hour) => {
    override = hour;
    apply();
  }, override));

  const resize = () => {
    const [w, h] = size();
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
    renderer.setSize(w, h);
    requestRender();
  };
  const resizer = new ResizeObserver(resize);
  resizer.observe(root);

  // Render only while the document and the office are both visible, at most 30 times a
  // second (uncapped while the camera is being moved), and only on demand under reduced
  // motion. `office.frames` counts the frames drawn.
  const FRAME_MS = 28;
  office.frames = 0;
  controls.addEventListener("change", () => {
    boostUntil = performance.now() + 250;
    requestRender();
  });
  if (reducedMotion) controls.enableDamping = false;
  function canRun() {
    return !disposed && onScreen && !document.hidden;
  }
  function schedule() {
    if (!frame && canRun()) frame = requestAnimationFrame(loop);
  }
  /// Under reduced motion nothing animates on its own: draw two frames (one to apply the
  /// change, one to settle) whenever something changes. Otherwise the loop is running anyway.
  function requestRender() {
    wanted = 2;
    schedule();
  }
  function loop(now) {
    frame = 0;
    if (!canRun()) return;
    if (!reducedMotion) {
      schedule();
      if (now - lastFrame < FRAME_MS && now > boostUntil) return;
    } else if (--wanted > 0) {
      schedule();
    }
    lastFrame = now;
    const dt = Math.min(clock.getDelta(), 0.1);
    agents.tick(dt, clock.elapsedTime);
    handles.tick(dt, clock.elapsedTime);
    stepFocus(clock.elapsedTime);
    controls.update();
    renderer.render(scene, camera);
    office.frames += 1;
    if (!office.ready) office.ready = true;
  }
  function sync() {
    if (canRun()) {
      if (!frame) clock.getDelta();
      requestRender();
    } else if (frame) {
      cancelAnimationFrame(frame);
      frame = 0;
    }
  }
  const seen = new IntersectionObserver((entries) => {
    onScreen = entries.some((e) => e.isIntersecting);
    sync();
  });
  seen.observe(root);
  document.addEventListener("visibilitychange", sync);

  function dispose() {
    if (disposed) return;
    disposed = true;
    sync();
    clearInterval(timer);
    live.stop();
    panel.close();
    seen.disconnect();
    resizer.disconnect();
    document.removeEventListener("visibilitychange", sync);
    document.removeEventListener("loopx:tool", onTool);
    controls.dispose();
    disposeScene(scene);
    renderer.dispose();
  }
  canvas.addEventListener("webglcontextlost", (e) => {
    e.preventDefault();
    dispose();
    showList("context_lost");
  });
  office.dispose = dispose;
  addEventListener("pagehide", dispose);
  // Back/forward cache: the scene was disposed on pagehide, so rebuild it.
  addEventListener("pageshow", (e) => {
    if (e.persisted && disposed) location.reload();
  });
  sync();
}

main().catch((err) => {
  console.error(err);
  showList("load_failed");
});
