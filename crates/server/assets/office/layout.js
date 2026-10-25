// Where people go, generated from the loop count: two long shared tables with
// seats on both sides, and the lounge, "Needs you" and standby spot sets. Ported
// from the prototype (`desks`, `seatAt`, `longTable`, `LOUNGE`/`WAIT`/`OFF`); up to
// 12 loops get the prototype's tables and seat spacing, more loops lengthen the
// tables and add spots. `buildLayout` is plain data; `furnish` draws the tables.

const { LOUNGE, WAIT, WAIT_MAT, OFF, BENCH, rng } = await import("./world.js" + new URL(import.meta.url).search);

/// The most people drawn; extra loops are only counted (`overflow`).
export const MAX_DRAWN = 24;

/// The four seat rows: table centre line, which side of it, and the aisle walked
/// to reach the row (between the tables, behind table A, in front of table B).
const ROWS = [
  { zc: -3, side: 1, za: 0.5 },
  { zc: -3, side: -1, za: -6.3 },
  { zc: 4, side: -1, za: 0.5 },
  { zc: 4, side: 1, za: 8 },
];

/// Lounge spots beyond the prototype's eleven, used only when there are more
/// people: four more places at the coffee bar and two standing groups.
const LOUNGE_EXTRA = [
  ...[6.4, 8.4, 10.4, 11.4].map((x) => ({ kind: "coffee", x, z: -8.2, face: [x, -12], via: [[3, -7]] })),
  ...ring(11.2, -5.8, 1.1, 5).map(([x, z]) => ({ kind: "chat", x, z, face: [11.2, -5.8], via: [[3, -6]] })),
  ...ring(5.4, 6.2, 1.0, 4).map(([x, z]) => ({ kind: "chat", x, z, face: [5.4, 6.2], via: [[3, 6.2]] })),
];
/// A second, staggered row on the "Needs you" mat.
const WAIT_EXTRA = [0.1, 1.5, 2.9, 4.3].map((x) => ({ kind: "wait", x, z: 9.2, face: [x, 40], via: [] }));
/// Standby beyond the bench: standing on its grey mat, then a queue at the front left.
const OFF_EXTRA = [
  ...[5.95, 7.05, 8.15, 9.25].map((z) => ({ kind: "off", x: -13.2, z, face: [0, z], via: [] })),
  ...[10.4, 11.4].flatMap((z) => [-12, -10.9, -9.8, -8.7].map((x) => ({ kind: "off", x, z, face: [x, 40], via: [[x + 0.55, 8], [x + 0.55, z]] }))),
];

function ring(cx, cz, r, n) {
  return Array.from({ length: n }, (_, i) => {
    const a = (i / n) * Math.PI * 2 + 0.4;
    return [cx + Math.cos(a) * r, cz + Math.sin(a) * r];
  });
}

const byDistance = (spots, at, axis = "x") => [...spots].sort((a, b) => Math.abs(a[axis] - at) - Math.abs(b[axis] - at));
const copy = (spots) => spots.map((s) => ({ ...s, occ: null }));

/// The layout for `count` loops: `tables` [{cx, zc, len}], `seats` in assignment
/// order (row by row, left to right), the `lounge`/`wait`/`off` spot sets and how
/// many loops are left undrawn. A seat is `{x, z, zc, za, gap}`: the person sits at
/// (x, z) facing the table centre line zc, reached through the gap at x + gap
/// between chairs and the row's aisle za. `furnish` adds each seat's `screenMat`.
export function buildLayout(count) {
  const n = Math.max(0, Math.floor(count) || 0);
  const drawn = Math.min(n, MAX_DRAWN);
  const perSide = Math.min(6, Math.max(3, Math.ceil(drawn / 4)));
  // 2.2 m apart as in the prototype; closer when six a side must fit on the carpet
  // and still leave the walk past the standby bench (x = -12.8) clear.
  const spacing = perSide <= 4 ? 2.2 : 9.6 / perSide;
  const len = perSide * spacing + 0.6;
  const cx = Math.min(-7.8, -2.4 - len / 2);
  const tables = [-3, 4].map((zc) => ({ cx, zc, len }));
  const seats = [];
  for (const row of ROWS) {
    for (let i = 0; i < perSide; i++) {
      const x = cx + (i - (perSide - 1) / 2) * spacing;
      seats.push({ x, z: row.zc + row.side * 1.5, zc: row.zc, za: row.za, side: row.side, gap: spacing / 2, screenMat: null, occ: null });
    }
  }
  // Extra spots come in steps of four, so one more loop rarely changes the layout (and
  // so rebuilds it and moves everyone): only the seat tier and these tiers do.
  const tier = (base) => Math.ceil(Math.max(0, drawn - base) / 4);
  const extra = (list, base) => list.slice(0, tier(base) * 4);
  return {
    key: `${perSide}:${tier(LOUNGE.length)}:${tier(WAIT.length)}:${tier(OFF.length)}`,
    tables,
    seats,
    lounge: copy([...LOUNGE, ...extra(LOUNGE_EXTRA, LOUNGE.length)]),
    // Mat and bench fill from the middle: people take the first free spot.
    wait: copy([...byDistance(WAIT, WAIT_MAT.x), ...extra(WAIT_EXTRA, WAIT.length)]),
    off: copy([...byDistance(OFF, BENCH.z, "z"), ...extra(OFF_EXTRA, OFF.length)]),
    overflow: n - drawn,
  };
}

/// Draws the layout's tables, table-top plants and vases, monitors, keyboards and
/// chairs into a new group added to `scene`, and gives each seat its `screenMat`.
/// Returns the group, so a later layout can replace it.
export function furnish(scene, THREE, layout, seed = 11) {
  const R = rng(seed);
  const group = new THREE.Group();
  scene.add(group);
  const M = (c, o = {}) => new THREE.MeshStandardMaterial({ color: c, roughness: 0.7, metalness: 0.05, ...o });
  const leafM = (c) => M(c, { roughness: 0.8 });
  function box(w, h, d, mat, x, y, z, parent = group) {
    const m = new THREE.Mesh(new THREE.BoxGeometry(w, h, d), mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    parent.add(m);
    return m;
  }
  function cyl(r1, r2, h, mat, x, y, z, parent = group, seg = 16) {
    const m = new THREE.Mesh(new THREE.CylinderGeometry(r1, r2, h, seg), mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    parent.add(m);
    return m;
  }
  function blob(x, y, z, r, c) {
    const m = new THREE.Mesh(new THREE.SphereGeometry(r, 12, 9), leafM(c));
    m.position.set(x, y, z);
    m.castShadow = true;
    group.add(m);
    return m;
  }
  function miniPlant(x, z) {
    cyl(0.12, 0.1, 0.2, M(0xb5651d), x, 0.94, z, group, 10);
    for (let i = 0; i < 4; i++) blob(x + (R() - 0.5) * 0.1, 1.12 + i * 0.04, z + (R() - 0.5) * 0.1, 0.1, [0x2f8f4e, 0x3d9f55][i % 2]);
  }
  function succulent(x, y, z) {
    cyl(0.1, 0.08, 0.12, M(0xd9cfc0), x, y + 0.06, z, group, 10);
    for (let i = 0; i < 7; i++) {
      const a = (i / 7) * 6.28, c = new THREE.Mesh(new THREE.ConeGeometry(0.04, 0.16, 6), leafM(i % 2 ? 0x6fbf8a : 0x4f9f6a));
      c.position.set(x + Math.cos(a) * 0.05, y + 0.2, z + Math.sin(a) * 0.05);
      c.rotation.z = Math.cos(a) * 0.7;
      c.rotation.x = -Math.sin(a) * 0.7;
      group.add(c);
    }
  }
  function cactus(x, y, z) {
    cyl(0.1, 0.08, 0.12, M(0xc4673a), x, y + 0.06, z, group, 10);
    cyl(0.045, 0.05, 0.3, leafM(0x4f9a5a), x, y + 0.27, z, group, 10);
    cyl(0.03, 0.03, 0.14, leafM(0x4f9a5a), x + 0.07, y + 0.28, z, group, 8);
  }
  function vase(x, z) {
    cyl(0.07, 0.06, 0.3, new THREE.MeshStandardMaterial({ color: 0xcfe9ff, transparent: true, opacity: 0.5, roughness: 0.1 }), x, 0.99, z, group, 10);
    for (let i = 0; i < 5; i++) blob(x + (R() - 0.5) * 0.12, 1.2 + R() * 0.1, z + (R() - 0.5) * 0.12, 0.06, [0xf4a6c3, 0xffd24a, 0xffffff, 0xff7a7a][i % 4]);
  }

  const top = M(0xcfa66e, { roughness: 0.4 }), steel = M(0x4a4f5a);
  for (const { cx, zc, len } of layout.tables) {
    box(len, 0.08, 2.0, top, cx, 0.8, zc);
    for (const sx of [-1, 1]) for (const sz of [-1, 1]) box(0.1, 0.8, 0.1, steel, cx + sx * (len / 2 - 0.2), 0.4, zc + sz * 0.85);
    box(len - 0.6, 0.04, 0.04, steel, cx, 0.35, zc + 0.85);
    box(len - 0.6, 0.04, 0.04, steel, cx, 0.35, zc - 0.85);
    // Plants and flower vases down the middle.
    const n = Math.floor(len / 1.3);
    for (let i = 0; i < n; i++) {
      const px = cx - len / 2 + (len / n) * (i + 0.5);
      [() => miniPlant(px, zc), () => succulent(px, 0.84, zc), () => vase(px, zc), () => cactus(px, 0.84, zc)][i % 4]();
    }
  }

  const black = M(0x222222), bezel = M(0x15171b), keys = M(0x2a2d34), seatMat = M(0x30343d), legMat = M(0x888888), mugMat = M(0xffffff);
  for (const seat of layout.seats) {
    const st = new THREE.Group();
    st.position.set(seat.x, 0, seat.zc);
    st.rotation.y = seat.side === 1 ? 0 : Math.PI;
    group.add(st);
    box(0.1, 0.3, 0.1, black, 0, 0.99, 0.35, st);
    box(0.6, 0.04, 0.3, black, 0, 0.84, 0.35, st);
    box(1.05, 0.65, 0.05, bezel, 0, 1.4, 0.35, st);
    seat.screenMat = new THREE.MeshStandardMaterial({ color: 0x111111, emissive: 0x000000 });
    const screen = new THREE.Mesh(new THREE.PlaneGeometry(0.95, 0.55), seat.screenMat);
    screen.position.set(0, 1.4, 0.38);
    st.add(screen);
    box(0.6, 0.03, 0.2, keys, 0, 0.86, 0.78, st);
    box(0.09, 0.03, 0.14, keys, 0.5, 0.86, 0.78, st);
    if (R() > 0.5) cyl(0.07, 0.06, 0.14, mugMat, -0.75, 0.91, 0.55, st, 12);
    // Chair.
    box(0.8, 0.1, 0.8, seatMat, 0, 0.5, 1.5, st);
    box(0.8, 0.8, 0.1, seatMat, 0, 1.0, 1.92, st);
    box(0.08, 0.45, 0.08, legMat, 0, 0.25, 1.5, st);
    box(0.7, 0.05, 0.05, legMat, 0, 0.05, 1.5, st);
    box(0.05, 0.05, 0.7, legMat, 0, 0.05, 1.5, st);
  }
  return group;
}
