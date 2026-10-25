// The office's fixed world: the room shell, lounge furniture, plants, art, the AI
// centre rack, the standby bench and "Needs you" mat, and everything outside the
// windows. Tables, chairs and monitors are not built here (they depend on the loop
// count). Ported from the approved prototype; every coordinate is in metres with
// the back wall at z = ROOM.Z0 and the windows looking north (towards -z).

const art = await import("./art.js" + new URL(import.meta.url).search);

export const ROOM = { W: 30, Z0: -10.5, Z1: 12.5 };
const D = ROOM.Z1 - ROOM.Z0, ZC = (ROOM.Z0 + ROOM.Z1) / 2;

/// The blue office carpet the work tables stand on (x0..x1, z0..z1).
export const CARPET = { x0: -14.5, x1: -1.5, z0: ZC - 11, z1: ZC + 11 };
/// Corridor points people walk through between zones: the lounge hub and the
/// central corridor.
export const HUB = [3, 1];
export const COR = [0, 1];
/// Spots in the lounge: where an idle person stands or sits, what they face and
/// the waypoints that lead there from HUB. `sitY` marks a seat.
export const LOUNGE = [
  { kind: "coffee", x: 7.4, z: -8.2, face: [7.4, -12], via: [[3, -7]] },
  { kind: "coffee", x: 9.4, z: -8.2, face: [9.4, -12], via: [[3, -7]] },
  { kind: "chat", x: 6.7, z: -3, face: [8, -3], via: [[3, -3]] },
  { kind: "chat", x: 8, z: -4.3, face: [8, -3], via: [[3, -3]] },
  { kind: "chat", x: 8, z: -1.7, face: [8, -3], via: [] },
  { kind: "chat", x: 9.3, z: -3, face: [8, -3], via: [[9.3, -1]] },
  { kind: "sofa", sitY: -0.45, x: 13.2, z: 3.2, face: [10.8, 4], via: [[12.2, 1.5], [12.2, 3.2]] },
  { kind: "sofa", sitY: -0.45, x: 13.2, z: 4.8, face: [10.8, 4], via: [[12.2, 1.5], [12.2, 4.8]] },
  { kind: "sofa", sitY: -0.45, x: 10.0, z: 7.9, face: [10.8, 4], via: [[8.5, 6.2], [10, 6.4]] },
  { kind: "sofa", sitY: -0.45, x: 11.6, z: 7.9, face: [10.8, 4], via: [[8.5, 6.2], [11.6, 6.4]] },
  { kind: "sofa", sitY: -0.45, x: 8.2, z: 4.0, face: [10.8, 4], via: [[8.2, 2.2]] },
];
/// The "Needs you" mat: where a person waiting for approval stands, facing the viewer.
export const WAIT_MAT = { x: 2.2, z: 9.9, w: 7, d: 1.8 };
export const WAIT = [-0.6, 0.8, 2.2, 3.6, 5.0].map((x) => ({ kind: "wait", x, z: 9.9, face: [x, 40], via: [] }));
/// The standby bench on the left wall, for disabled loops.
export const BENCH = { x: -14.4, z: 7.4, len: 5.4 };
export const OFF = [5.4, 6.5, 7.6, 8.7, 9.8].map((z) => ({ kind: "off", sitY: -0.45, x: -14.3, z, face: [0, z], via: [] }));
/// The AI centre rack (against the back wall) and the point its beams start from.
export const AI_CENTRE = { x: 0, z: -9.1 };

export function rng(seed) {
  return () => {
    seed |= 0;
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/// The scene's lights: sky/ground fill, the sun (casts shadows), a warm lounge
/// lamp and four ceiling lights over the work and waiting areas that come on at night.
/// Stops recomputing the matrices of everything under `root` (the root too, unless it is a
/// scene) except the subtrees in `movers`: for objects that never move after being built.
export function freezeStatic(root, movers = []) {
  const skip = new Set(movers);
  root.updateMatrixWorld(true);
  const walk = (o) => {
    for (const c of o.children) {
      if (skip.has(c)) continue;
      if (!c.isLight) c.matrixAutoUpdate = false;
      walk(c);
    }
  };
  if (!root.isScene) root.matrixAutoUpdate = false;
  walk(root);
}

export function buildLights(scene, THREE) {
  const hemi = new THREE.HemisphereLight(0xdfe8ff, 0x6b5a45, 0.8);
  scene.add(hemi);
  const sun = new THREE.DirectionalLight(0xfff1dd, 1.0);
  sun.position.set(8, 26, 16);
  sun.castShadow = true;
  sun.shadow.mapSize.set(2048, 2048);
  Object.assign(sun.shadow.camera, { left: -24, right: 24, top: 24, bottom: -24, near: 1, far: 70 });
  scene.add(sun);
  const warm = new THREE.PointLight(0xffc27a, 40, 18);
  warm.position.set(9, 4.5, 3);
  scene.add(warm);
  const tableLights = [[-7.8, 5.5, -3], [-7.8, 5.5, 4], [9, 5.5, 3], [2.2, 5.5, 9]].map((p) => {
    const l = new THREE.PointLight(0xffe2b0, 0, 16);
    l.position.set(...p);
    scene.add(l);
    return l;
  });
  return { hemi, sun, warm, tableLights };
}

/// Builds the world into `scene`. `opts.strings` is the translated client string
/// table (for the two signs); `opts.reducedMotion` leaves clouds, cars, birds,
/// walkers and the rack LEDs still.
export function buildWorld(scene, THREE, opts = {}) {
  const strings = opts.strings || {};
  const still = !!opts.reducedMotion;
  const R = rng(opts.seed || 7);
  const { W, Z0 } = ROOM;

  const M = (c, o = {}) => new THREE.MeshStandardMaterial({ color: c, roughness: 0.7, metalness: 0.05, ...o });
  const bm = (c) => new THREE.MeshBasicMaterial({ color: c });
  function box(w, h, d, mat, x = 0, y = 0, z = 0, parent = scene) {
    const m = new THREE.Mesh(new THREE.BoxGeometry(w, h, d), mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    parent.add(m);
    return m;
  }
  function cyl(r1, r2, h, mat, x, y, z, parent = scene, seg = 16) {
    const m = new THREE.Mesh(new THREE.CylinderGeometry(r1, r2, h, seg), mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    parent.add(m);
    return m;
  }
  // Outdoor meshes: no shadows, they are far away.
  function om(geo, mat, x, y, z, parent = scene) {
    const m = new THREE.Mesh(geo, mat);
    m.position.set(x, y, z);
    parent.add(m);
    return m;
  }
  const jitter = (hex, a = 0.03, l = 0.06) => {
    const c = new THREE.Color(hex);
    c.offsetHSL((R() - 0.5) * a * 2, 0, (R() - 0.5) * l * 2);
    return c;
  };
  function sprite(tex, w, h, x, y, z) {
    const s = new THREE.Sprite(new THREE.SpriteMaterial({ map: tex, transparent: true, depthWrite: false }));
    s.scale.set(w, h, 1);
    s.position.set(x, y, z);
    s.renderOrder = 10;
    scene.add(s);
    return s;
  }

  // ---------- room ----------
  box(W, 0.3, D, M(0x9a7a56, { roughness: 0.55 }), 0, -0.15, ZC);
  // Back wall with six real window openings: the outdoors shows through them.
  const wallMat = M(0xe9e6df);
  const WIN_W = 2.8, WIN_Y0 = 2.5, WIN_Y1 = 5.5, WINX = [-12, -8, -4, 6.5, 10, 13.5];
  {
    let cur = -W / 2;
    const frame = M(0xf7f5f0, { roughness: 0.5 });
    const glass = new THREE.MeshStandardMaterial({ color: 0xcfe9ff, transparent: true, opacity: 0.14, roughness: 0.05, metalness: 0.1, depthWrite: false });
    WINX.forEach((wx) => {
      const a = wx - WIN_W / 2;
      box(a - cur, 7, 0.3, wallMat, (cur + a) / 2, 3.5, Z0 - 0.15);
      box(WIN_W, WIN_Y0, 0.3, wallMat, wx, WIN_Y0 / 2, Z0 - 0.15);
      box(WIN_W, 7 - WIN_Y1, 0.3, wallMat, wx, (WIN_Y1 + 7) / 2, Z0 - 0.15);
      const h = WIN_Y1 - WIN_Y0, cy = (WIN_Y0 + WIN_Y1) / 2, zf = Z0 + 0.02;
      box(WIN_W + 0.2, 0.12, 0.12, frame, wx, WIN_Y1, zf);
      box(WIN_W + 0.2, 0.12, 0.12, frame, wx, WIN_Y0, zf);
      box(0.12, h, 0.12, frame, wx - WIN_W / 2, cy, zf);
      box(0.12, h, 0.12, frame, wx + WIN_W / 2, cy, zf);
      box(0.07, h, 0.07, frame, wx, cy, zf);
      box(WIN_W, 0.07, 0.07, frame, wx, cy + 0.2, zf);
      const g = new THREE.Mesh(new THREE.PlaneGeometry(WIN_W, h), glass);
      g.position.set(wx, cy, Z0 - 0.15);
      scene.add(g);
      box(WIN_W + 0.4, 0.07, 0.4, frame, wx, WIN_Y0 - 0.02, Z0 + 0.18); // sill
      cur = wx + WIN_W / 2;
    });
    box(W / 2 - cur, 7, 0.3, wallMat, (cur + W / 2) / 2, 3.5, Z0 - 0.15);
  }
  box(0.3, 7, D, M(0xe3e0d8), -W / 2 - 0.15, 3.5, ZC);
  box(0.3, 7, D, M(0xe3e0d8), W / 2 + 0.15, 3.5, ZC);
  box(W, 0.25, 0.1, M(0x6e5a42), 0, 0.12, Z0 + 0.05);
  // Office-zone carpet and the round lounge rug.
  box(CARPET.x1 - CARPET.x0, 0.02, CARPET.z1 - CARPET.z0, M(0x39465c, { roughness: 1 }), (CARPET.x0 + CARPET.x1) / 2, 0.01, ZC).castShadow = false;
  cyl(6.2, 6.2, 0.03, M(0x8c4a3c, { roughness: 1 }), 9.5, 0.02, 3, scene, 48).castShadow = false;
  cyl(4.9, 4.9, 0.035, M(0xd9c7a0, { roughness: 1 }), 9.5, 0.025, 3, scene, 48).castShadow = false;

  // ---------- indoor plants, several kinds ----------
  const leafM = (c) => M(c, { roughness: 0.8 });
  function blob(x, y, z, r, c, sx = 1, sy = 1, sz = 1, parent = scene) {
    const m = new THREE.Mesh(new THREE.SphereGeometry(r, 12, 9), leafM(c));
    m.position.set(x, y, z);
    m.scale.set(sx, sy, sz);
    m.castShadow = true;
    parent.add(m);
    return m;
  }
  const pot = (x, z, s, c = 0xb5651d) => cyl(0.4 * s, 0.32 * s, 0.65 * s, M(c), x, 0.325 * s, z);
  function ficus(x, z, s = 1) {
    pot(x, z, s);
    for (let i = 0; i < 5; i++) blob(x + (R() - 0.5) * 0.5 * s, (1.1 + i * 0.2) * s, z + (R() - 0.5) * 0.5 * s, (0.45 + R() * 0.2) * s, [0x2f8f4e, 0x3d9f55, 0x2a7d45][i % 3]);
  }
  function palm(x, z, s = 1) {
    pot(x, z, s, 0x8a8f98);
    cyl(0.09 * s, 0.15 * s, 2.2 * s, M(0x7a5a3a), x, 1.7 * s, z);
    for (let i = 0; i < 9; i++) {
      const a = (i / 9) * Math.PI * 2;
      const l = blob(x + Math.cos(a) * 0.95 * s, 2.75 * s, z + Math.sin(a) * 0.95 * s, 1, i % 2 ? 0x2f9b52 : 0x44b266, 1.1 * s, 0.05 * s, 0.26 * s);
      l.rotation.order = "YZX";
      l.rotation.y = -a;
      l.rotation.z = -0.45;
    }
  }
  function snake(x, z, s = 1) {
    pot(x, z, s, 0x3a3f4a);
    for (let i = 0; i < 9; i++) {
      const h = (0.9 + R() * 0.8) * s;
      const l = box(0.17 * s, h, 0.05 * s, leafM(i % 3 ? 0x2c6e3f : 0x5c9a3a), x + (R() - 0.5) * 0.5 * s, 0.55 * s + h / 2, z + (R() - 0.5) * 0.4 * s);
      l.rotation.y = R() * Math.PI;
      l.rotation.z = (R() - 0.5) * 0.3;
    }
  }
  function fiddle(x, z, s = 1) {
    pot(x, z, s, 0xd9cfc0);
    cyl(0.05 * s, 0.08 * s, 2.2 * s, M(0x6b4a2b), x, 1.7 * s, z);
    for (let i = 0; i < 11; i++) {
      const a = i * 2.4, y = (1.2 + i * 0.17) * s;
      const l = blob(x + Math.cos(a) * 0.4 * s, y, z + Math.sin(a) * 0.4 * s, 0.5 * s, 0x2f7d3a, 0.9, 0.12, 0.65);
      l.rotation.y = -a;
      l.rotation.z = 0.4;
    }
  }
  function monstera(x, z, s = 1) {
    pot(x, z, s, 0x6a4a30);
    for (let i = 0; i < 10; i++) {
      const a = (i / 10) * Math.PI * 2, y = (0.9 + (i % 3) * 0.28) * s;
      const l = blob(x + Math.cos(a) * 0.6 * s, y, z + Math.sin(a) * 0.6 * s, 0.55 * s, [0x1f7a45, 0x2f9a55][i % 2], 1, 0.07, 0.8);
      l.rotation.order = "YZX";
      l.rotation.y = -a;
      l.rotation.z = 0.5;
    }
  }
  function flowering(x, z, s = 1) {
    pot(x, z, s, 0xc0c4cc);
    for (let i = 0; i < 4; i++) blob(x + (R() - 0.5) * 0.6 * s, (1 + i * 0.12) * s, z + (R() - 0.5) * 0.6 * s, 0.5 * s, 0x3f9a4a);
    for (let i = 0; i < 18; i++) {
      const a = R() * 6.28, r = R() * 0.7 * s;
      blob(x + Math.cos(a) * r, (1.2 + R() * 0.5) * s, z + Math.sin(a) * r, 0.09 * s, [0xf4a6c3, 0xffffff, 0xffd24a][i % 3]);
    }
  }
  ficus(-1.2, 11.7, 0.9);
  ficus(14, 11.7);
  palm(-14, -9.7, 1.1);
  snake(-2.8, -9.7, 1);
  fiddle(14.3, -8.3, 1);
  monstera(-14, 11.7, 1);
  flowering(14.3, 0, 0.9);
  palm(-14, 2.5, 0.8);
  snake(2.4, 11.7, 0.9);

  // ---------- lounge furniture ----------
  // Coffee bar on the back wall.
  box(7, 1, 0.9, M(0x6a4a30), 9, 0.5, -9.8);
  box(7.2, 0.08, 1.0, M(0xe8e4dc), 9, 1.04, -9.8);
  box(0.7, 0.7, 0.6, M(0x2b2e36), 7.6, 1.43, -9.9); // machine
  box(0.5, 0.14, 0.4, M(0xb3261e), 7.6, 1.85, -9.9);
  cyl(0.2, 0.2, 0.45, M(0xcfd3da, { metalness: 0.6, roughness: 0.3 }), 9.4, 1.3, -9.8); // kettle
  for (let i = 0; i < 4; i++) cyl(0.09, 0.08, 0.17, M(0xffffff), 10.4 + i * 0.28, 1.18, -9.7);
  box(1.6, 2.4, 0.5, M(0x5b3f2a), 3.4, 1.2, -10.1); // bookshelf
  for (let i = 0; i < 8; i++) box(0.14, 0.45, 0.3, M([0xc44b4b, 0x4b79c4, 0xd1a84b, 0x4bc48a][i % 4]), 2.8 + i * 0.17, 1.95 - (i % 2) * 0.8, -9.95);
  cyl(0.35, 0.35, 1.1, M(0xdfe9f5, { roughness: 0.2 }), 13, 0.55, -9.6); // water cooler
  cyl(0.3, 0.3, 0.5, M(0x7fc4ff, { roughness: 0.1, transparent: true, opacity: 0.7 }), 13, 1.35, -9.6);
  // High round table.
  cyl(0.65, 0.65, 0.06, M(0xe8e4dc), 8, 1.1, -3);
  cyl(0.07, 0.07, 1.1, M(0x333333), 8, 0.55, -3);
  cyl(0.4, 0.4, 0.05, M(0x333333), 8, 0.03, -3);
  // Sofas, armchair, coffee table.
  const sofaMat = M(0x3f6f8f, { roughness: 0.9 }), sofa2 = M(0x8f5a3f, { roughness: 0.9 }), chairMat = M(0x6a8f3d, { roughness: 0.9 });
  box(1.0, 0.5, 3.4, sofaMat, 13.2, 0.25, 4.0);
  box(0.25, 0.9, 3.4, sofaMat, 13.85, 0.7, 4.0);
  box(1.0, 0.3, 0.25, sofaMat, 13.2, 0.65, 2.35);
  box(1.0, 0.3, 0.25, sofaMat, 13.2, 0.65, 5.65);
  box(3.4, 0.5, 1.0, sofa2, 10.8, 0.25, 7.9);
  box(3.4, 0.9, 0.25, sofa2, 10.8, 0.7, 8.55);
  box(0.25, 0.3, 1.0, sofa2, 9.1, 0.65, 7.9);
  box(0.25, 0.3, 1.0, sofa2, 12.5, 0.65, 7.9);
  box(1.0, 0.5, 1.0, chairMat, 8.2, 0.25, 4.0);
  box(0.25, 0.9, 1.0, chairMat, 7.75, 0.7, 4.0);
  const wood = M(0x4a3a2a);
  box(1.6, 0.08, 1.1, wood, 10.8, 0.45, 4.0);
  for (const [lx, lz] of [[10.2, 3.6], [11.4, 3.6], [10.2, 4.4], [11.4, 4.4]]) box(0.08, 0.4, 0.08, wood, lx, 0.2, lz);
  cyl(0.07, 0.07, 0.12, M(0xffffff), 10.5, 0.55, 4.1);
  cyl(0.07, 0.07, 0.12, M(0xffffff), 11.1, 0.55, 3.9);
  // Floor lamp; its shade glows brighter at night.
  cyl(0.03, 0.03, 2.4, M(0x333333), 14, 1.2, 0.8);
  const lampShade = cyl(0.45, 0.3, 0.5, M(0xfff0c8, { emissive: 0xffd88a, emissiveIntensity: 0.8 }), 14, 2.5, 0.8).material;

  // ---------- the LoopX logo on the back wall, right of centre ----------
  {
    const p = new THREE.Mesh(
      new THREE.PlaneGeometry(7, 2.4),
      new THREE.MeshBasicMaterial({ map: art.logoTexture(THREE), transparent: true, depthWrite: false, toneMapped: false })
    );
    p.position.set(1.4, 4.2, Z0 + 0.11);
    scene.add(p);
  }

  // ---------- standby bench for disabled loops (left wall) ----------
  const benchMat = M(0x5a5f6b);
  box(1.0, 0.45, BENCH.len, benchMat, BENCH.x, 0.22, BENCH.z);
  box(0.25, 0.9, BENCH.len, benchMat, BENCH.x - 0.45, 0.7, BENCH.z);
  box(0.1, 0.45, 0.1, M(0x333333), -14.0, 0.22, BENCH.z - 2.5);
  box(0.1, 0.45, 0.1, M(0x333333), -14.0, 0.22, BENCH.z + 2.5);
  box(1.4, 0.02, 6.2, M(0x7b7f8a), -13.6, 0.03, BENCH.z).castShadow = false; // grey mat
  if (strings.disabled_standby) {
    const tex = art.signTexture(THREE, strings.disabled_standby, { width: 480, height: 128, background: "rgba(60,64,74,.95)", color: "#e6e8ee", size: 56 });
    sprite(tex, 3.0, 0.8, -13.2, 3.4, 4.6); // clear of the wall, which would clip it
  }

  // ---------- the "Needs you" mat and its icon ----------
  box(WAIT_MAT.w, 0.02, WAIT_MAT.d, M(0xffb020, { emissive: 0x553500 }), WAIT_MAT.x, 0.03, WAIT_MAT.z).castShadow = false;
  // Just a small hand icon above the mat, no wording.
  {
    const tex = art.signTexture(THREE, "✋", { width: 128, height: 128, background: "rgba(138,90,0,.95)", color: "#fff", size: 80 });
    sprite(tex, 0.9, 0.9, WAIT_MAT.x, 3.2, 10.6);
  }

  // ---------- AI centre: three racks of blinking LEDs, no label ----------
  const aiLeds = [];
  const aiCore = new THREE.Group();
  aiCore.position.set(AI_CENTRE.x, 0, AI_CENTRE.z);
  scene.add(aiCore);
  const aiLight = new THREE.PointLight(0x5c8cff, 14, 10);
  aiLight.position.set(0, 2.8, 0.6);
  aiCore.add(aiLight);
  const aiTop = new THREE.Vector3(AI_CENTRE.x, 2.6, AI_CENTRE.z + 0.5);
  {
    const body = M(0x1d2230, { roughness: 0.4, metalness: 0.5 });
    const door = new THREE.MeshStandardMaterial({ color: 0x0b0e16, roughness: 0.2, metalness: 0.6 });
    [-1.15, 0, 1.15].forEach((rx) => {
      box(1.0, 2.5, 0.9, body, rx, 1.25, 0, aiCore);
      box(0.9, 2.3, 0.02, door, rx, 1.25, 0.46, aiCore);
      for (let r = 0; r < 12; r++) {
        for (let c = 0; c < 3; c++) {
          const mat = new THREE.MeshStandardMaterial({ color: 0x111111, emissive: [0x3a8bff, 0x7a5cff, 0x3ddc84][(r + c) % 3], emissiveIntensity: 0.6 });
          box(0.2, 0.1, 0.02, mat, rx - 0.28 + c * 0.28, 0.3 + r * 0.18, 0.48, aiCore).castShadow = false;
          aiLeds.push({ mat, p: R() * 10, s: 1 + R() * 3 });
        }
      }
    });
  }

  // ---------- paintings inside, graffiti on the outer side walls ----------
  const frameColours = [0x2b2b2b, 0xf2f2f2, 0xb8914a, 0x6a4a30];
  for (const [kind, x, y, z, w, h, rotY] of art.paintingPlacements(Z0)) {
    const g = new THREE.Group();
    g.position.set(x, y, z);
    g.rotation.y = rotY;
    scene.add(g);
    box(w + 0.2, h + 0.2, 0.08, M(frameColours[Math.floor(R() * 4)], { roughness: 0.5 }), 0, 0, 0, g);
    const p = new THREE.Mesh(new THREE.PlaneGeometry(w, h), new THREE.MeshBasicMaterial({ map: art.paintingTexture(THREE, kind, R), toneMapped: false }));
    p.position.z = 0.05;
    g.add(p);
  }
  for (const wall of art.GRAFFITI) {
    const tex = art.graffitiTexture(THREE, wall.words, wall.extras, R);
    const m = new THREE.Mesh(new THREE.PlaneGeometry(D, 7), new THREE.MeshBasicMaterial({ map: tex, transparent: true, depthWrite: false, toneMapped: false }));
    m.position.set(wall.x, 3.5, ZC);
    m.rotation.y = wall.rotY;
    scene.add(m);
  }

  // ---------- sky, sun, moon, stars (animated by daynight.js) ----------
  const skyCanvas = document.createElement("canvas");
  skyCanvas.width = 16;
  skyCanvas.height = 256;
  const skyTex = new THREE.CanvasTexture(skyCanvas);
  skyTex.colorSpace = THREE.SRGBColorSpace;
  om(new THREE.SphereGeometry(190, 32, 16), new THREE.MeshBasicMaterial({ map: skyTex, side: THREE.BackSide, depthWrite: false, fog: false }), 0, 0, 0);
  function haloTex(rgb) {
    const c = document.createElement("canvas");
    c.width = c.height = 128;
    const x = c.getContext("2d");
    const g = x.createRadialGradient(64, 64, 0, 64, 64, 64);
    g.addColorStop(0, "rgba(" + rgb + ",.9)");
    g.addColorStop(1, "rgba(" + rgb + ",0)");
    x.fillStyle = g;
    x.fillRect(0, 0, 128, 128);
    return new THREE.CanvasTexture(c);
  }
  function halo(rgb, size) {
    const s = new THREE.Sprite(new THREE.SpriteMaterial({ map: haloTex(rgb), transparent: true, depthWrite: false }));
    s.scale.set(size, size, 1);
    scene.add(s);
    return s;
  }
  const sunMesh = om(new THREE.SphereGeometry(7, 24, 16), bm(0xfff4b8), 0, 0, -150);
  const sunHalo = halo("255,244,184", 60);
  const moonMesh = om(new THREE.SphereGeometry(5, 24, 16), bm(0xe8ecff), 0, 0, -150);
  const moonHalo = halo("170,190,255", 40);
  const stars = (() => {
    const n = 700, pos = new Float32Array(n * 3);
    for (let i = 0; i < n; i++) {
      const th = R() * 6.283, ph = Math.acos(0.05 + R() * 0.95);
      pos[i * 3] = 170 * Math.sin(ph) * Math.cos(th);
      pos[i * 3 + 1] = 170 * Math.cos(ph);
      pos[i * 3 + 2] = 170 * Math.sin(ph) * Math.sin(th);
    }
    const g = new THREE.BufferGeometry();
    g.setAttribute("position", new THREE.BufferAttribute(pos, 3));
    const p = new THREE.Points(g, new THREE.PointsMaterial({ color: 0xffffff, size: 1.6, sizeAttenuation: false, transparent: true, opacity: 0, depthWrite: false }));
    scene.add(p);
    return p;
  })();

  // ---------- the ground outside ----------
  const grass = new THREE.Mesh(new THREE.PlaneGeometry(400, 400), M(0x75b455, { roughness: 1 }));
  grass.rotation.x = -Math.PI / 2;
  grass.position.y = -0.32;
  grass.receiveShadow = true;
  scene.add(grass);
  const pavement = M(0xcfcfd2);
  om(new THREE.BoxGeometry(300, 0.04, 2.6), pavement, 0, -0.3, -12.6); // sidewalk
  om(new THREE.BoxGeometry(300, 0.04, 9), M(0x3b3f46), 0, -0.28, -40); // road
  const dash = bm(0xf2f2f2), dashGeo = new THREE.BoxGeometry(2.5, 0.045, 0.2);
  for (let x = -140; x <= 140; x += 6) om(dashGeo, dash, x, -0.27, -40);
  om(new THREE.BoxGeometry(300, 0.04, 2.6), pavement, 0, -0.3, -46.5);

  // Trees of six kinds.
  function tree(type, x, z, s) {
    const g = new THREE.Group();
    g.position.set(x, -0.3, z);
    g.scale.setScalar(s);
    scene.add(g);
    const th = type === "poplar" ? 3 : 2.4;
    om(new THREE.CylinderGeometry(0.16, 0.26, th, 8), M(type === "birch" ? 0xeeeae0 : 0x6b4a2b), 0, th / 2, 0, g);
    const F = (r, y, cx, cz, col, sx = 1, sy = 1, sz = 1) => {
      om(new THREE.SphereGeometry(r, 12, 9), M(jitter(col), { roughness: 0.9 }), cx, y, cz, g).scale.set(sx, sy, sz);
    };
    if (type === "round") {
      F(1.7, 3.8, 0, 0, 0x3f9a4a);
      F(1.3, 3.3, 1, 0.4, 0x4aa655);
      F(1.3, 3.4, -1, -0.3, 0x358c44);
    } else if (type === "pine") {
      [[1.8, 2.4, 3.0], [1.4, 2.2, 4.3], [1.0, 2.0, 5.5]].forEach(([r, h, y]) => om(new THREE.ConeGeometry(r, h, 10), M(jitter(0x1f6b3d), { roughness: 0.9 }), 0, y, 0, g));
    } else if (type === "birch") {
      F(1.1, 4.3, 0, 0, 0x8fcf5a, 1, 1.5, 1);
      F(0.8, 3.5, 0.6, 0.2, 0x9bd868);
    } else if (type === "cherry") {
      F(1.7, 3.8, 0, 0, 0xf4a6c3);
      F(1.3, 3.4, 1, 0.3, 0xffc2d6);
      F(1.2, 3.5, -1, -0.2, 0xf08fb2);
    } else if (type === "autumn") {
      F(1.7, 3.8, 0, 0, 0xe0732a);
      F(1.3, 3.3, 1, 0.4, 0xe8a830);
      F(1.3, 3.4, -1, -0.3, 0xc9402a);
    } else {
      F(0.9, 4.5, 0, 0, 0x2f7a45, 1, 2.4, 1);
    }
  }
  [["cherry", -12, -15, 1.1], ["pine", -8.5, -17, 1.2], ["round", -3.5, -15.5, 1], ["birch", 2, -16, 1.1], ["autumn", 6.5, -15, 1], ["poplar", 10, -17, 1.2],
    ["round", 14, -16, 1.1], ["pine", -18, -18, 1.3], ["birch", -22, -15, 1], ["cherry", 20, -17, 1], ["autumn", -30, -20, 1.2], ["round", 26, -20, 1.2]]
    .forEach((a) => tree(...a));
  {
    const types = ["round", "pine", "birch", "cherry", "autumn", "poplar"];
    for (let i = 0; i < 30; i++) tree(types[Math.floor(R() * 6)], (R() - 0.5) * 140, -20 - R() * 14, 0.9 + R() * 0.6);
  }
  // Flowerbed and hedge under the windows.
  for (let x = -14.5; x <= 14.5; x += 1.6) {
    if (R() > 0.2) om(new THREE.SphereGeometry(0.5 + R() * 0.3, 10, 8), M(jitter(0x3a8f4a), { roughness: 1 }), x, 0.1, -11.5);
    if (R() > 0.5) {
      for (let j = 0; j < 4; j++) {
        om(new THREE.SphereGeometry(0.09, 6, 6), bm([0xff6b6b, 0xffd24a, 0xffffff, 0xc77dff][Math.floor(R() * 4)]), x + (R() - 0.5) * 0.8, 0.45 + R() * 0.3, -11.2 + R() * 0.3);
      }
    }
  }
  // Benches and lamp posts on the sidewalk.
  const benchWood = M(0x7a5a3a);
  for (const bx of [-6, 11]) {
    box(2, 0.1, 0.6, benchWood, bx, 0.25, -13.7).castShadow = false;
    box(2, 0.5, 0.08, benchWood, bx, 0.6, -13.98).castShadow = false;
  }
  const lampM = bm(0xfff3c4);
  for (const x of [-1, 8, -14]) {
    om(new THREE.CylinderGeometry(0.05, 0.07, 3.4, 8), M(0x333333), x, 1.4, -13.3);
    om(new THREE.SphereGeometry(0.22, 10, 8), lampM, x, 3.2, -13.3);
  }
  // Skyline: day windows in the colour map, night windows in the emissive map.
  function windowsTexture(lit) {
    const c = document.createElement("canvas");
    c.width = 64;
    c.height = 128;
    const x = c.getContext("2d");
    x.fillStyle = lit ? "#000" : "#cfd6df";
    x.fillRect(0, 0, 64, 128);
    for (let r = 0; r < 8; r++) {
      for (let q = 0; q < 4; q++) {
        if (lit) {
          if (R() > 0.4) {
            x.fillStyle = R() > 0.5 ? "#ffe39a" : "#ffd070";
            x.fillRect(q * 16 + 3, r * 16 + 3, 10, 10);
          }
        } else {
          x.fillStyle = R() > 0.8 ? "#ffe9a8" : "#5d7a99";
          x.fillRect(q * 16 + 3, r * 16 + 3, 10, 10);
        }
      }
    }
    const t = new THREE.CanvasTexture(c);
    t.wrapS = t.wrapT = THREE.RepeatWrapping;
    t.colorSpace = THREE.SRGBColorSpace;
    return t;
  }
  const buildingMats = [];
  const dayWindows = windowsTexture(false), nightWindows = windowsTexture(true);
  for (let x = -110; x <= 110; x += 11) {
    const w = 7 + R() * 6, h = 12 + R() * 26;
    const t = dayWindows.clone();
    t.needsUpdate = true;
    t.repeat.set(Math.max(1, Math.round(w / 4)), Math.max(1, Math.round(h / 8)));
    const te = nightWindows.clone();
    te.needsUpdate = true;
    te.repeat.copy(t.repeat);
    const mat = new THREE.MeshStandardMaterial({ map: t, color: jitter(0xdfe6ee, 0.02, 0.08), roughness: 0.8, emissive: 0xffffff, emissiveMap: te, emissiveIntensity: 0 });
    buildingMats.push(mat);
    om(new THREE.BoxGeometry(w, h, 9), mat, x + (R() - 0.5) * 3, h / 2 - 0.3, -62 - R() * 10);
  }
  // Hills behind the city.
  [[-70, -135, 80, 0x6f9fa0], [10, -140, 100, 0x7aa6a8], [95, -130, 70, 0x6f9fa0], [-135, -125, 60, 0x78a389]].forEach(([x, z, r, c]) => {
    om(new THREE.SphereGeometry(1, 24, 12), M(c, { roughness: 1 }), x, -4, z).scale.set(r, r * 0.22, r * 0.5);
  });

  // ---------- moving things: clouds, cars, birds, walkers ----------
  const clouds = [], cars = [], birds = [], walkers = [];
  const cloudM = new THREE.MeshBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.92 });
  const carLightM = bm(0xcfd3da);
  for (let i = 0; i < 10; i++) {
    const g = new THREE.Group(), n = 4 + Math.floor(R() * 3);
    for (let j = 0; j < n; j++) om(new THREE.SphereGeometry(3 + R() * 3, 10, 8), cloudM, j * 4.2 - n * 2, R() * 2, R() * 3, g).scale.y = 0.55;
    g.position.set((R() - 0.5) * 260, 30 + R() * 18, -80 - R() * 70);
    scene.add(g);
    clouds.push(g);
  }
  function car(col) {
    const g = new THREE.Group();
    om(new THREE.BoxGeometry(2.6, 0.75, 1.2), M(col, { roughness: 0.4 }), 0, 0.65, 0, g);
    om(new THREE.BoxGeometry(1.4, 0.55, 1.05), M(0xbfd8ee, { roughness: 0.1 }), -0.1, 1.25, 0, g);
    for (const sx of [-0.85, 0.85]) for (const sz of [-0.6, 0.6]) om(new THREE.CylinderGeometry(0.3, 0.3, 0.2, 12), M(0x111111), sx, 0.3, sz, g).rotation.x = Math.PI / 2;
    for (const sx of [-1.3, 1.3]) for (const sz of [-0.4, 0.4]) om(new THREE.BoxGeometry(0.06, 0.14, 0.22), carLightM, sx, 0.7, sz, g);
    return g;
  }
  for (let i = 0; i < 8; i++) {
    const g = car([0xd1453b, 0x3b6fd1, 0xf2f2f2, 0xe0b020, 0x2f2f33, 0x3bb273][i % 6]), lane = i % 2;
    g.position.set((R() - 0.5) * 200, -0.3, lane ? -38.4 : -41.8);
    if (!lane) g.rotation.y = Math.PI;
    scene.add(g);
    cars.push({ g, v: (4 + R() * 5) * (lane ? 1 : -1) });
  }
  const birdM = bm(0x333333), wingGeo = new THREE.BoxGeometry(0.5, 0.02, 0.18);
  function placeBird(b, t) {
    b.g.position.set(b.cx + Math.cos(b.a) * b.r, b.y + Math.sin(b.a * 3) * 0.8, b.cz + Math.sin(b.a) * b.r * 0.5);
    b.g.rotation.y = Math.atan2(-Math.sin(b.a) * b.r, Math.cos(b.a) * b.r * 0.5);
    const f = Math.sin(t * 10 + b.a * 5) * 0.7;
    b.wl.rotation.z = f;
    b.wr.rotation.z = -f;
  }
  for (let i = 0; i < 7; i++) {
    const g = new THREE.Group(), wl = new THREE.Group(), wr = new THREE.Group();
    om(wingGeo, birdM, 0.25, 0, 0, wr);
    om(wingGeo, birdM, -0.25, 0, 0, wl);
    g.add(wl, wr);
    om(new THREE.SphereGeometry(0.1, 6, 6), birdM, 0, 0, 0, g);
    scene.add(g);
    const b = { g, wl, wr, cx: (R() - 0.5) * 60, cz: -55 - R() * 25, r: 14 + R() * 14, y: 12 + R() * 12, sp: 0.15 + R() * 0.2, a: R() * 6.28 };
    placeBird(b, 0);
    birds.push(b);
  }
  function walker(col, x, dir) {
    const g = new THREE.Group();
    om(new THREE.CylinderGeometry(0.2, 0.2, 0.8, 8), M(col), 0, 1.2, 0, g);
    om(new THREE.SphereGeometry(0.18, 10, 8), M(0xe0b48f), 0, 1.8, 0, g);
    const lg = new THREE.BoxGeometry(0.12, 0.8, 0.12).translate(0, -0.4, 0), legM = M(0x2c3140);
    const l = om(lg, legM, -0.09, 0.8, 0, g), r = om(lg, legM, 0.09, 0.8, 0, g);
    g.position.set(x, -0.3, -12.6);
    g.rotation.y = dir > 0 ? Math.PI / 2 : -Math.PI / 2;
    scene.add(g);
    walkers.push({ g, l, r, v: dir * (1 + R() * 0.6) });
  }
  walker(0xd1604b, -10, 1);
  walker(0x4b79c4, 6, -1);
  walker(0x5bb58a, 18, 1);

  let env = { day: 1, tw: 0 };
  const C = THREE.Color, mix = (a, b, t) => new C(a).lerp(new C(b), t), lerp = (a, b, t) => a + (b - a) * t;

  const handles = {
    clouds, cars, birds, walkers,
    lamps: { posts: lampM, shade: lampShade },
    carLights: carLightM,
    buildingMats,
    grass: grass.material,
    cloudMat: cloudM,
    aiTop,
    aiLight,
    sky: { canvas: skyCanvas, ctx: skyCanvas.getContext("2d"), tex: skyTex },
    sun: { mesh: sunMesh, halo: sunHalo },
    moon: { mesh: moonMesh, halo: moonHalo },
    stars,
    /// How busy the AI centre looks: its LEDs blink faster with more people working.
    aiRate: 1,
    /// Recolours the world for `env.day` (0 night .. 1 day) and `env.tw` (twilight glow).
    setNight(next) {
      env = next;
      const dayF = env.day, tw = env.tw || 0;
      lampShade.emissiveIntensity = lerp(1.8, 0.2, dayF);
      lampM.color.copy(mix(0xfff3a0, 0xbfc4cc, dayF));
      carLightM.color.copy(mix(0xfff2a0, 0xcfd3da, dayF));
      grass.material.color.copy(mix(0x24402a, 0x75b455, dayF)).lerp(new C(0x8a6a3c), tw * 0.12);
      cloudM.color.copy(mix(0x39425f, 0xffffff, dayF)).lerp(new C(0xffb08a), tw * 0.5);
      cloudM.opacity = lerp(0.7, 0.92, dayF);
      for (const m of buildingMats) m.emissiveIntensity = 1 - dayF;
      for (const b of birds) b.g.visible = dayF > 0.35;
    },
    /// Advances the ambient motion by `dt` seconds (`t` = seconds since start).
    tick(dt, t) {
      for (const l of aiLeds) l.mat.emissiveIntensity = Math.sin((still ? 0 : t) * l.s * handles.aiRate + l.p) > 0.1 ? 1.4 : 0.12;
      if (still) return;
      for (const c of clouds) {
        c.position.x += dt * 0.8;
        if (c.position.x > 150) c.position.x = -150;
      }
      for (const c of cars) {
        c.g.position.x += c.v * dt;
        if (c.g.position.x > 115) c.g.position.x = -115;
        if (c.g.position.x < -115) c.g.position.x = 115;
      }
      for (const b of birds) {
        b.a += b.sp * dt;
        placeBird(b, t);
      }
      for (const w of walkers) {
        w.g.position.x += w.v * dt;
        if (w.g.position.x > 45) w.g.position.x = -45;
        if (w.g.position.x < -45) w.g.position.x = 45;
        const s = Math.sin(t * 6 * Math.abs(w.v)) * 0.6;
        w.l.rotation.x = s;
        w.r.rotation.x = -s;
      }
    },
  };
  return { handles };
}
