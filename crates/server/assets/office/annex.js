// The wings of the office that are not the round desk: on the west side a meeting nook with a
// whiteboard and a row of spare workstations, on the east a ping-pong table, a library wall with
// reading chairs and bean bags, a vending machine, a printer and a fridge. The same file holds
// what the colleagues need to walk about: `FLOOR` (where they may stand), `OBSTACLES` (circles
// to walk round), `STOPS` (places worth strolling to) and `planPath`, which routes a walk round
// the obstacles. All coordinates are in metres, like world.js.

/// The walkable floor: nobody strolls outside it.
export const FLOOR = { x0: -23.2, x1: 23.2, z0: -9.0, z1: 12.2 };

/// Circles `[x, z, radius]` a walk goes round: the round desk with its chairs, the plants, the
/// lounge furniture, the racks and everything the wings add.
export const OBSTACLES = [
  [-8, 1, 4.5],
  // plants
  [-14, -9.7, 0.9], [-14, 2.5, 0.8], [-14, 11.7, 0.9], [-1.2, 11.7, 0.8], [-2.8, -9.7, 0.8], [2.4, 11.7, 0.8], [14, 11.7, 0.9], [14.3, -8.3, 0.8], [14.3, 0, 0.9],
  // the back wall: AI racks, bookshelf, coffee bar, water cooler
  [-1.15, -9.1, 0.9], [0, -9.1, 0.9], [1.15, -9.1, 0.9], [3.4, -9.9, 1.0], [6.4, -9.5, 1.0], [8.2, -9.5, 1.0], [10, -9.5, 1.0], [11.8, -9.5, 1.0], [13, -9.5, 0.7],
  // lounge: high table, sofas, armchair, coffee table, floor lamp
  [8, -3, 0.9], [13.4, 2.7, 0.9], [13.4, 4, 1.1], [13.4, 5.3, 0.9], [9.7, 8, 0.9], [10.8, 8, 1.1], [12, 8, 0.9], [8.2, 4, 0.8], [10.8, 4, 1.2], [14, 0.8, 0.6],
  // west wing: meeting table and chairs, workstations
  [-20.6, -6, 1.35], [-19.4, -6, 1.35], [-22.9, 3.2, 1.2], [-22.9, 5.8, 1.2], [-22.9, 8.4, 1.2], [-22.9, 11, 1.2],
  // east wing: ping-pong table, library chairs and bean bags, vending machine, printer, fridge
  [19.5, -4.1, 1.2], [19.5, -3, 1.2], [19.5, -1.9, 1.2], [21.6, 5, 0.8], [21.6, 9.4, 0.8], [18.6, 9.8, 0.9], [20.3, 10.8, 0.9],
  [20.5, -9.6, 1.0], [17.5, -9.6, 1.0], [22.5, -9.6, 0.9],
];

/// Places to stroll to: `[x, z, faceX, faceZ, pose]`. `pose` is how they stand there: `stand`,
/// `read` (a book open in both hands), `sip` (a mug), `play` (swinging a paddle) or `think`.
export const STOPS = [
  // the back wall
  [0, -7.5, 0, -9.1, "think"], [3.4, -8.3, 3.4, -9.9, "read"], [8.2, -8.2, 8.2, -9.6, "sip"], [11.8, -8.2, 11.8, -9.6, "sip"], [13, -8.1, 13, -9.5, "sip"],
  // the lounge
  [8, -1.8, 8, -3, "sip"], [11.5, 1.6, 11.5, 4, "stand"], [9, 6.2, 10.8, 4, "stand"], [15.5, 3.8, 20, 3.8, "stand"],
  // the middle of the office
  [1.5, 4.5, 6, 4.5, "stand"], [-0.5, 9.5, -0.5, 14, "think"], [4.5, 10, 4.5, 14, "think"],
  // the west wing
  [-22.1, -6, -24, -6, "think"], [-15.4, -6, -16.4, -6, "stand"], [-20, -3.4, -20, -6, "stand"], [-20, -8.6, -20, -6, "stand"],
  [-20.4, 3.2, -22.9, 3.2, "stand"], [-20.4, 8.4, -22.9, 8.4, "stand"], [-20.4, 11, -22.9, 11, "stand"], [-18, 6.5, -23, 6.5, "stand"],
  // the east wing
  [19.5, -6.1, 19.5, -3, "play"], [19.5, 0.1, 19.5, -3, "play"], [20.5, -8.1, 20.5, -9.6, "stand"], [17.5, -8.1, 17.5, -9.6, "stand"], [22.5, -8.1, 22.5, -9.6, "sip"],
  [22.1, 3.4, 24, 3.4, "read"], [22.1, 7.2, 24, 7.2, "read"], [22.1, 11, 24, 11, "read"], [17.4, 7.6, 21.6, 7.6, "stand"],
];

const CELL = 0.5, MARGIN = 0.25;
const COLS = Math.ceil((FLOOR.x1 - FLOOR.x0) / CELL) + 1, ROWS = Math.ceil((FLOOR.z1 - FLOOR.z0) / CELL) + 1;
const cellCentre = (c, r) => [FLOOR.x0 + c * CELL, FLOOR.z0 + r * CELL];
const blockedAt = (x, z, obstacles) => obstacles.some((o) => Math.hypot(x - o[0], z - o[1]) < o[2] + MARGIN);
/// Whether the straight walk between two points keeps clear of every obstacle.
function clear(a, b, obstacles) {
  const n = Math.max(1, Math.ceil(Math.hypot(b[0] - a[0], b[1] - a[1]) / 0.2));
  for (let i = 0; i <= n; i++) {
    const t = i / n;
    if (blockedAt(a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, obstacles.filter((o) => Math.hypot(a[0] - o[0], a[1] - o[1]) >= o[2] || Math.hypot(b[0] - o[0], b[1] - o[1]) >= o[2]))) return false;
  }
  return true;
}

/// The points to walk through to get from `from` (`[x, z]`) to `to`, `to` last: a straight line
/// when nothing is in the way, else the shortest route round the obstacles on a half-metre grid,
/// pulled taut. Where either end stands inside an obstacle's margin the route starts or ends at
/// the nearest free cell.
export function planPath(from, to, obstacles = OBSTACLES) {
  if (clear(from, to, obstacles)) return [to];
  const id = (c, r) => r * COLS + c;
  const cellOf = (p) => [Math.max(0, Math.min(COLS - 1, Math.round((p[0] - FLOOR.x0) / CELL))), Math.max(0, Math.min(ROWS - 1, Math.round((p[1] - FLOOR.z0) / CELL)))];
  const free = (c, r) => !blockedAt(...cellCentre(c, r), obstacles);
  /// The free cell nearest `p`, searching outwards.
  const nearestFree = (p) => {
    const [c0, r0] = cellOf(p);
    for (let d = 0; d < Math.max(COLS, ROWS); d++) {
      let best = null;
      for (let r = r0 - d; r <= r0 + d; r++) {
        for (let c = c0 - d; c <= c0 + d; c++) {
          if (Math.max(Math.abs(c - c0), Math.abs(r - r0)) !== d || c < 0 || r < 0 || c >= COLS || r >= ROWS || !free(c, r)) continue;
          const [x, z] = cellCentre(c, r), dd = Math.hypot(x - p[0], z - p[1]);
          if (!best || dd < best.dd) best = { c, r, dd };
        }
      }
      if (best) return [best.c, best.r];
    }
    return [c0, r0];
  };
  const [sc, sr] = nearestFree(from), [gc, gr] = nearestFree(to);
  const h = (c, r) => Math.hypot(c - gc, r - gr);
  const g = new Map([[id(sc, sr), 0]]), came = new Map(), open = [[h(sc, sr), sc, sr]], done = new Set();
  const push = (e) => {
    open.push(e);
    for (let i = open.length - 1; i > 0;) {
      const up = (i - 1) >> 1;
      if (open[up][0] <= open[i][0]) break;
      [open[up], open[i]] = [open[i], open[up]];
      i = up;
    }
  };
  const pop = () => {
    const top = open[0], last = open.pop();
    if (open.length) {
      open[0] = last;
      for (let i = 0; ;) {
        const l = 2 * i + 1, r = l + 1;
        let m = i;
        if (l < open.length && open[l][0] < open[m][0]) m = l;
        if (r < open.length && open[r][0] < open[m][0]) m = r;
        if (m === i) break;
        [open[m], open[i]] = [open[i], open[m]];
        i = m;
      }
    }
    return top;
  };
  open.length = 0;
  push([h(sc, sr), sc, sr]);
  let found = false;
  while (open.length) {
    const [, c, r] = pop();
    const k = id(c, r);
    if (done.has(k)) continue;
    done.add(k);
    if (c === gc && r === gr) {
      found = true;
      break;
    }
    for (let dc = -1; dc <= 1; dc++) {
      for (let dr = -1; dr <= 1; dr++) {
        const nc = c + dc, nr = r + dr;
        if ((!dc && !dr) || nc < 0 || nr < 0 || nc >= COLS || nr >= ROWS || !free(nc, nr)) continue;
        // No cutting a corner between two blocked cells.
        if (dc && dr && (!free(c + dc, r) || !free(c, r + dr))) continue;
        const nk = id(nc, nr), cost = g.get(k) + (dc && dr ? 1.4142 : 1);
        if (cost < (g.has(nk) ? g.get(nk) : Infinity)) {
          g.set(nk, cost);
          came.set(nk, k);
          push([cost + h(nc, nr), nc, nr]);
        }
      }
    }
  }
  if (!found) return [to];
  const cells = [];
  for (let k = id(gc, gr); k !== undefined; k = came.get(k)) cells.push(cellCentre(k % COLS, Math.floor(k / COLS)));
  cells.reverse();
  // Pull taut: from each point, jump to the furthest later point still in clear sight.
  const pts = [from, ...cells, to], out = [];
  let at = 0;
  while (at < pts.length - 1) {
    let next = at + 1;
    for (let i = pts.length - 1; i > at + 1; i--) {
      if (clear(pts[at], pts[i], obstacles)) {
        next = i;
        break;
      }
    }
    out.push(pts[next]);
    at = next;
  }
  return out;
}

/// Draws the wings into `scene`. `ctx`: {M (material factory), R (seeded random)}.
export function buildAnnex(scene, THREE, { M, R }) {
  const group = new THREE.Group();
  scene.add(group);
  const box = (w, h, d, mat, x, y, z) => {
    const m = new THREE.Mesh(new THREE.BoxGeometry(w, h, d), mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    group.add(m);
    return m;
  };
  const cyl = (r1, r2, h, mat, x, y, z, seg = 14) => {
    const m = new THREE.Mesh(new THREE.CylinderGeometry(r1, r2, h, seg), mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    group.add(m);
    return m;
  };
  const flat = (w, d, mat, x, z) => {
    box(w, 0.02, d, mat, x, 0.012, z).castShadow = false;
  };
  const steel = M(0x4a4f5a), wood = M(0x5b3f2a), white = M(0xf2f2f2), dark = M(0x23262d);
  const standby = () => new THREE.MeshStandardMaterial({ color: 0x0d1018, emissive: 0x1d3a66, emissiveIntensity: 0.35 });
  const chairColours = [0x30343d, 0x3f6f8f, 0x8f5a3f, 0x6a8f3d].map((c) => M(c));

  function bush(x, z, s = 1) {
    cyl(0.4 * s, 0.32 * s, 0.65 * s, M(0xb5651d), x, 0.325 * s, z);
    for (let i = 0; i < 5; i++) {
      const b = new THREE.Mesh(new THREE.SphereGeometry((0.45 + R() * 0.2) * s, 12, 9), M([0x2f8f4e, 0x3d9f55, 0x2a7d45][i % 3], { roughness: 0.8 }));
      b.position.set(x + (R() - 0.5) * 0.5 * s, (1.1 + i * 0.2) * s, z + (R() - 0.5) * 0.5 * s);
      b.castShadow = true;
      group.add(b);
    }
  }
  /// An office chair at (x, z) whose back is on the `back` side (+1/-1) along x, or along z when `alongZ`.
  function chair(x, z, mat, back, alongZ = false) {
    box(0.55, 0.1, 0.55, mat, x, 0.48, z);
    box(0.08, 0.4, 0.08, steel, x, 0.25, z);
    box(0.55, 0.05, 0.05, steel, x, 0.05, z);
    box(0.05, 0.05, 0.55, steel, x, 0.05, z);
    if (alongZ) box(0.55, 0.7, 0.08, mat, x, 0.88, z + back * 0.24);
    else box(0.08, 0.7, 0.55, mat, x + back * 0.24, 0.88, z);
  }

  // ---------- west wing: the meeting nook ----------
  flat(7, 6, M(0x2f5f66, { roughness: 1 }), -20, -6);
  box(3.2, 0.08, 1.5, M(0xcfa66e, { roughness: 0.4 }), -20, 0.8, -6);
  for (const [lx, lz] of [[-21.4, -6.5], [-18.6, -6.5], [-21.4, -5.5], [-18.6, -5.5]]) box(0.08, 0.78, 0.08, steel, lx, 0.39, lz);
  [-21.2, -20, -18.8].forEach((x, i) => {
    chair(x, -7.2, chairColours[i % 4], -1, true);
    chair(x, -4.8, chairColours[(i + 1) % 4], 1, true);
  });
  chair(-17.6, -6, chairColours[2], 1);
  // The whiteboard on the west wall with a few sticky notes, and its marker tray.
  box(0.06, 1.9, 3.6, M(0xb8bcc4, { metalness: 0.4 }), -23.9, 2.2, -6);
  box(0.02, 1.7, 3.4, white, -23.85, 2.2, -6).castShadow = false;
  box(0.1, 0.05, 1.2, steel, -23.8, 1.25, -6);
  [[-6.9, 2.7, 0xffd24a], [-6.4, 2.35, 0xff8dba], [-5.4, 2.65, 0x7ac4ff], [-4.9, 2.2, 0x9be27a], [-6.2, 1.85, 0xffd24a], [-5.7, 2.05, 0xff8dba]].forEach(([z, y, c]) => {
    box(0.02, 0.26, 0.26, M(c), -23.8, y, z).castShadow = false;
  });
  // A screen on a stand at the end of the table.
  box(0.08, 1.0, 1.7, dark, -16.4, 1.55, -6);
  box(0.04, 0.9, 1.55, standby(), -16.43, 1.55, -6);
  box(0.1, 1.0, 0.1, steel, -16.3, 0.5, -6);
  box(0.5, 0.05, 0.9, steel, -16.3, 0.03, -6);

  // ---------- west wing: spare workstations ----------
  flat(4.4, 10.4, M(0x39465c, { roughness: 1 }), -21.9, 7.1);
  [3.2, 5.8, 8.4, 11].forEach((z, i) => {
    box(0.9, 0.06, 1.7, M(0xe8e4dc, { roughness: 0.45 }), -23.1, 0.76, z);
    box(0.06, 0.74, 1.6, steel, -22.7, 0.37, z);
    box(0.06, 0.74, 1.6, steel, -23.5, 0.37, z);
    box(0.05, 0.55, 0.9, dark, -23.45, 1.2, z);
    box(0.02, 0.48, 0.82, standby(), -23.42, 1.2, z).castShadow = false;
    box(0.1, 0.28, 0.1, steel, -23.4, 0.93, z);
    box(0.2, 0.03, 0.55, M(0x2a2d34), -22.8, 0.8, z);
    if (i % 2 === 0) cyl(0.07, 0.06, 0.14, M(0xffffff), -22.9, 0.86, z + 0.7, 12);
    chair(-21.9, z, chairColours[i % 4], 1);
    if (i < 3) box(1.6, 1.1, 0.06, M(0x7a8594), -22.9, 0.95, z + 1.3);
  });
  [-9.2, -8.5].forEach((z) => box(0.6, 1.3, 0.6, M(0x7a8594), -23.5, 0.65, z));
  bush(-17.2, 11.7, 0.9);
  bush(-23, -9.7, 1);

  // ---------- east wing: ping-pong ----------
  flat(5, 7.4, M(0x2f4a3a, { roughness: 1 }), 19.5, -3);
  box(1.5, 0.05, 2.74, M(0x1f6f4a, { roughness: 0.5 }), 19.5, 0.78, -3);
  box(0.03, 0.005, 2.74, white, 19.5, 0.81, -3).castShadow = false;
  box(1.5, 0.005, 0.03, white, 19.5, 0.81, -3).castShadow = false;
  box(1.62, 0.16, 0.02, M(0xe8ecf0, { transparent: true, opacity: 0.8 }), 19.5, 0.9, -3);
  for (const [lx, lz] of [[18.85, -4.2], [20.15, -4.2], [18.85, -1.8], [20.15, -1.8]]) box(0.07, 0.78, 0.07, steel, lx, 0.39, lz);
  for (const [px, pz, c] of [[19.1, -4.1, 0xd1453b], [19.9, -1.9, 0x3b6fd1]]) box(0.16, 0.012, 0.2, M(c), px, 0.82, pz).castShadow = false;

  // ---------- east wing: vending machine, printer, fridge ----------
  box(0.95, 1.95, 0.85, M(0x2b4f9a), 20.5, 0.98, -9.7);
  box(0.7, 1.2, 0.02, new THREE.MeshStandardMaterial({ color: 0xcfe9ff, emissive: 0x88aaff, emissiveIntensity: 0.45, roughness: 0.2 }), 20.45, 1.25, -9.27).castShadow = false;
  for (let r = 0; r < 4; r++) for (let c = 0; c < 4; c++) box(0.12, 0.2, 0.06, M([0xd1453b, 0xffd24a, 0x4bc48a, 0xf2f2f2][(r + c) % 4]), 20.2 + c * 0.17, 1.65 - r * 0.28, -9.3).castShadow = false;
  box(0.7, 0.18, 0.02, dark, 20.45, 0.45, -9.27);
  box(1.4, 0.75, 0.7, M(0x6a4a30), 17.5, 0.375, -9.7);
  box(0.8, 0.32, 0.5, M(0xcfd3da), 17.4, 0.91, -9.7);
  box(0.4, 0.04, 0.3, white, 17.8, 0.77, -9.4).castShadow = false;
  box(0.9, 1.9, 0.8, white, 22.5, 0.95, -9.7);
  box(0.05, 0.6, 0.05, steel, 22.1, 1.35, -9.25);

  // ---------- east wing: the library wall and reading chairs ----------
  flat(4.2, 9, M(0x7a4a3c, { roughness: 1 }), 21.4, 7.2);
  box(0.5, 2.4, 8.4, M(0x6b4a2b), 23.65, 1.2, 7.4);
  const palette = [0xc4574b, 0x4b79c4, 0xd1a84b, 0x4bc48a, 0x9a6ad1, 0xe8dcc8, 0x3d8f8f, 0xd17a4b].map((c) => M(c));
  let n = 0;
  for (let row = 0; row < 4; row++) {
    box(0.5, 0.04, 8.4, M(0x4a3422), 23.65, 0.45 + row * 0.55, 7.4).castShadow = false;
    for (let k = 0; k < 20; k++) {
      const h = 0.34 + ((k * 7 + row * 3) % 5) * 0.035;
      box(0.28, h, 0.3, palette[n++ % 8], 23.45, 0.47 + row * 0.55 + h / 2, 3.5 + k * 0.4).castShadow = false;
    }
  }
  for (const z of [5, 9.4]) {
    box(0.9, 0.5, 0.9, M(0x9b3d3d), 21.6, 0.28, z);
    box(0.2, 0.8, 0.9, M(0x9b3d3d), 22.1, 0.7, z);
    box(0.5, 0.05, 0.5, wood, 21.6, 0.5, z + 1.1);
  }
  cyl(0.03, 0.03, 2.2, steel, 22.6, 1.1, 7.2);
  cyl(0.4, 0.28, 0.45, new THREE.MeshStandardMaterial({ color: 0xfff0c8, emissive: 0xffd88a, emissiveIntensity: 0.6 }), 22.6, 2.3, 7.2);
  for (const [x, z, c] of [[18.6, 9.8, 0x4b79c4], [20.3, 10.8, 0xd1a84b]]) {
    const b = new THREE.Mesh(new THREE.SphereGeometry(0.62, 16, 12), M(c, { roughness: 0.95 }));
    b.scale.y = 0.62;
    b.position.set(x, 0.38, z);
    b.castShadow = true;
    group.add(b);
  }
  bush(23, -6.6, 1.1);
  bush(23, 11.7, 0.9);
  bush(16.2, -9.7, 0.9);

  return group;
}
