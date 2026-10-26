// Where the three colleagues sit: one round desk with a hollow centre holding a
// plant, the seats spaced evenly around it. `buildLayout` is plain data; `furnish`
// draws the desk, the plant, the chairs and a small screen in front of each seat.

const { rng } = await import("./world.js" + new URL(import.meta.url).search);

/// How many colleagues the office has, whatever the number of loops.
export const SEATS = 3;
/// The desk's centre on the carpet, its outer and inner radius, and how far out the seats are.
export const DESK = { x: -8, z: 1, outer: 3.0, inner: 1.1, seat: 3.6 };

/// Seat `i` of `SEATS`: at (x, z), facing the desk centre (`face`). Seat 0 is at the far
/// side so it faces the default camera; the others follow at 120 degrees.
export function buildLayout() {
  const seats = [];
  for (let i = 0; i < SEATS; i++) {
    const a = -Math.PI / 2 + (i * 2 * Math.PI) / SEATS;
    seats.push({
      x: DESK.x + Math.cos(a) * DESK.seat,
      z: DESK.z + Math.sin(a) * DESK.seat,
      angle: a,
      face: [DESK.x, DESK.z],
      screenMat: null,
      lit: false,
    });
  }
  return { key: "round", desk: DESK, seats };
}

/// Draws the layout into a new group added to `scene` and gives each seat its `screenMat`.
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
  const { x: cx, z: cz, outer, inner } = layout.desk;

  // The desk: a flat ring (a lathed rectangle), three legs between the seats, a brace ring.
  const steel = M(0x4a4f5a);
  const ring = (r0, r1, y0, y1) =>
    new THREE.LatheGeometry([[r0, y0], [r1, y0], [r1, y1], [r0, y1], [r0, y0]].map(([x, y]) => new THREE.Vector2(x, y)), 64);
  const slab = new THREE.Mesh(ring(inner, outer, 0.76, 0.84), new THREE.MeshStandardMaterial({ color: 0xcfa66e, roughness: 0.4, side: THREE.DoubleSide }));
  slab.position.set(cx, 0, cz);
  slab.castShadow = true;
  slab.receiveShadow = true;
  group.add(slab);
  for (let i = 0; i < layout.seats.length; i++) {
    const a = layout.seats[i].angle + Math.PI / layout.seats.length;
    const r = (outer + inner) / 2;
    box(0.1, 0.76, 0.1, steel, cx + Math.cos(a) * r, 0.38, cz + Math.sin(a) * r);
  }
  const brace = new THREE.Mesh(ring(inner + 0.05, outer - 0.25, 0.33, 0.37), new THREE.MeshStandardMaterial({ color: 0x4a4f5a, roughness: 0.7, side: THREE.DoubleSide }));
  brace.position.set(cx, 0, cz);
  group.add(brace);

  // The plant in the hollow centre: a planter on the floor and a leafy bush that rises above the desk.
  cyl(0.55, 0.45, 0.6, M(0xb5651d), cx, 0.3, cz, group, 20);
  cyl(0.5, 0.5, 0.04, M(0x3b2a1a), cx, 0.6, cz, group, 20);
  cyl(0.06, 0.08, 0.9, M(0x6b4a2b), cx, 1.05, cz, group, 8);
  const greens = [0x2f8f4e, 0x3d9f55, 0x4faa62, 0x2a7f46];
  for (let i = 0; i < 14; i++) {
    const a = R() * Math.PI * 2, rr = R() * 0.38, y = 1.35 + R() * 0.65;
    const m = new THREE.Mesh(new THREE.SphereGeometry(0.2 + R() * 0.12, 12, 9), leafM(greens[i % greens.length]));
    m.position.set(cx + Math.cos(a) * rr, y, cz + Math.sin(a) * rr);
    m.castShadow = true;
    group.add(m);
  }

  // Each seat: a screen on the desk facing the person, a keyboard, and a chair whose back is outward.
  const bezel = M(0x15171b), keys = M(0x2a2d34), seatMat = M(0x30343d), legMat = M(0x888888), mugMat = M(0xffffff);
  for (const seat of layout.seats) {
    // Local +z points away from the desk centre, so the person (who faces -z) looks at the plant.
    const yaw = Math.atan2(-(cx - seat.x), -(cz - seat.z));
    const st = new THREE.Group();
    st.position.set(seat.x, 0, seat.z);
    st.rotation.y = yaw;
    group.add(st);
    box(1.0, 0.6, 0.05, bezel, 0, 1.3, -1.3, st);
    seat.screenMat = new THREE.MeshStandardMaterial({ color: 0x111111, emissive: 0x000000 });
    const screen = new THREE.Mesh(new THREE.PlaneGeometry(0.9, 0.5), seat.screenMat);
    screen.position.set(0, 1.3, -1.27);
    st.add(screen);
    box(0.1, 0.3, 0.1, steel, 0, 0.99, -1.3, st);
    box(0.55, 0.03, 0.2, keys, 0, 0.86, -0.95, st);
    if (R() > 0.4) cyl(0.07, 0.06, 0.14, mugMat, -0.75, 0.91, -1.1, st, 12);
    box(0.8, 0.1, 0.8, seatMat, 0, 0.5, 0, st);
    box(0.8, 0.8, 0.1, seatMat, 0, 1.0, 0.42, st);
    box(0.08, 0.45, 0.08, legMat, 0, 0.25, 0, st);
    box(0.7, 0.05, 0.05, legMat, 0, 0.05, 0, st);
    box(0.05, 0.05, 0.7, legMat, 0, 0.05, 0, st);
  }
  return group;
}
