// Entry module for the Office view: checks WebGL (falling back to the List view
// with a note), mounts the renderer in #office, builds the world, runs day/night
// and the render loop, and seats one person per loop from `/office/state`. Test
// hooks in the URL hash: `h=<hour>` overrides the clock, `left`/`right`/`in` pick a
// camera (e.g. `#left&h=23`). `window.__office` exposes `debug()` (where everyone
// is), `setLoops(loops)` (draw these states instead), `overflow` and `syncs`.
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

/// Hides the office and shows the server-rendered List with a one-line note. The
/// `loop_view` cookie is left alone, so a browser with WebGL still gets the office.
function showList() {
  if (root) root.hidden = true;
  const list = document.getElementById("office-list");
  if (!list) return;
  if (!list.querySelector(".office-note")) {
    const note = document.createElement("p");
    note.className = "office-note";
    note.setAttribute("role", "status");
    note.textContent = strings.no_webgl || "";
    list.prepend(note);
  }
  list.hidden = false;
  for (const a of document.querySelectorAll(".view-toggle a")) {
    if (a.getAttribute("href").endsWith("view=list")) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
}

/// `{camera, hour}` from the URL hash, e.g. `#left&h=23`.
function hashHooks() {
  const parts = location.hash.replace(/^#/, "").split("&");
  const camera = ["left", "right", "in"].find((c) => parts.includes(c)) || null;
  const h = parts.map((p) => p.match(/^h=([\d.]+)$/)).find(Boolean);
  return { camera, hour: h ? Number(h[1]) : null };
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
  const [{ buildWorld, buildLights }, { createDayNight }, { createAgents }] = await Promise.all([
    import("./world.js" + V),
    import("./daynight.js" + V),
    import("./agents.js" + V),
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
  const dayNight = createDayNight(THREE, scene, handles, lights);
  const agents = createAgents({ THREE, scene, handles, strings, reducedMotion });
  // "and N more" for loops beyond the ones drawn; the List view has them all.
  const more = document.createElement("p");
  more.className = "office-more";
  more.hidden = true;
  root.append(more);
  const office = (window.__office = Object.assign(window.__office || {}, { overflow: 0, syncs: 0 }));
  function show(loops) {
    agents.sync(loops);
    const n = agents.overflow;
    more.textContent = n ? (strings.and_more || "").replace("{n}", String(n)) : "";
    more.hidden = !n;
    office.overflow = n;
    office.syncs += 1;
  }
  office.setLoops = show;
  office.debug = () => agents.debug();
  fetch("/office/state", { credentials: "same-origin", headers: { Accept: "application/json" } })
    .then((r) => (r.ok ? r.json() : Promise.reject(new Error("HTTP " + r.status))))
    .then((doc) => {
      if (!disposed) show(doc.loops);
    })
    .catch((err) => console.warn("office state:", err));
  let override = hooks.hour;
  const apply = () => dayNight.apply(dayNight.currentHour(override));
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
  };
  const resizer = new ResizeObserver(resize);
  resizer.observe(root);

  // Render only while the document and the office are both visible.
  const clock = new THREE.Clock();
  let frame = 0, onScreen = true, disposed = false;
  function loop() {
    frame = requestAnimationFrame(loop);
    const dt = Math.min(clock.getDelta(), 0.1);
    agents.tick(dt, clock.elapsedTime);
    handles.tick(dt, clock.elapsedTime);
    controls.update();
    renderer.render(scene, camera);
    if (!window.__office || !window.__office.ready) window.__office = Object.assign(window.__office || {}, { ready: true });
  }
  function sync() {
    const run = !disposed && onScreen && !document.hidden;
    if (run && !frame) {
      clock.getDelta();
      loop();
    } else if (!run && frame) {
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
    seen.disconnect();
    resizer.disconnect();
    document.removeEventListener("visibilitychange", sync);
    controls.dispose();
    disposeScene(scene);
    renderer.dispose();
  }
  canvas.addEventListener("webglcontextlost", (e) => {
    e.preventDefault();
    dispose();
    showList();
  });
  addEventListener("pagehide", dispose);
  // Back/forward cache: the scene was disposed on pagehide, so rebuild it.
  addEventListener("pageshow", (e) => {
    if (e.persisted && disposed) location.reload();
  });
  sync();
}

main().catch((err) => {
  console.error(err);
  showList();
});
