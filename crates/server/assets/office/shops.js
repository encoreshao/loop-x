// The two shops standing in a row with the office, far down the street to its west (out of the
// default view): coffee shop, bookstore, then the LoopX office, all with their fronts on the same street. Each is a roofless box with a glass front
// towards the viewer and a door in it, so the people inside can be seen. `SHOPS` is plain data
// (where the door is, where visitors and staff stand); `buildShops` draws them into the scene.

/// Shop footprint: `Z0` is the back wall, `Z1` the glass front and door side (the office's front edge).
const Z0 = 5.5, Z1 = 12.5, WIDTH = 8, WALL = 3.4, DOOR_W = 1.7;
/// Where the shops' rows of storefronts end (towards the office) and the street's lane points.
export const STREET = { kerb: Z1 + 0.05, walkZ: 14.4, doorApproachZ: Z1 + 1.1, roadZ0: 17.2, roadZ1: 25.2 };

/// For each shop: its centre `cx`, the door point, the spots the colleagues stand at (`spots`) and
/// the ones ambient customers use (`guests`), each facing something (`face`), and where the staff
/// member stands behind the counter.
export const SHOPS = {
  coffee: {
    cx: -74,
    spots: [[-1.6, 7.9, -1.6, 6.8], [0, 7.9, 0, 6.8], [1.6, 7.9, 1.6, 6.8]],
    guests: [[-2.6, 7.9, -2.6, 6.8], [2.6, 7.9, 2.6, 6.8]],
    staff: [0.9, 5.95],
  },
  books: {
    cx: -62,
    spots: [[-2.3, 9, -3.6, 9], [2.3, 9, 3.6, 9], [0, 10.7, 0, 9.4]],
    guests: [[-2.3, 7.4, -3.6, 7.4], [2.3, 7.4, 3.6, 7.4]],
    staff: [0, 5.95],
  },
};
for (const s of Object.values(SHOPS)) {
  const at = ([x, z, fx, fz]) => ({ x: s.cx + x, z, face: [s.cx + fx, fz] });
  s.door = [s.cx, Z1];
  s.spots = s.spots.map(at);
  s.guests = s.guests.map(at);
  s.staff = { x: s.cx + s.staff[0], z: s.staff[1], face: [s.cx + s.staff[0], Z1] };
}
/// The sidewalk lane between the office's front edge and the doors.
export const PLAZA_Z = STREET.walkZ;
export const DOOR_APPROACH_Z = STREET.doorApproachZ;

/// Draws the plaza and both shops into `scene`. `ctx`: {art (art.js), strings, M (material
/// factory), box (the world's box helper)}.
export function buildShops(scene, THREE, ctx) {
  const { art, strings, M } = ctx;
  const group = new THREE.Group();
  scene.add(group);
  const box = (w, h, d, mat, x, y, z, parent = group) => {
    const m = new THREE.Mesh(new THREE.BoxGeometry(w, h, d), mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    parent.add(m);
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

  // The street in front of the row: a wide sidewalk along every storefront, the kerb, then the
  // road with its lane markings (the cars and pedestrians that use it live in world.js).
  const paving = M(0xcfcfd2, { roughness: 0.9 });
  box(300, 0.06, STREET.roadZ0 - Z1 - 0.2, paving, 0, 0.0, (Z1 + STREET.roadZ0) / 2).castShadow = false;
  box(300, 0.1, 0.12, M(0xa9a9ae), 0, 0.05, STREET.roadZ0 - 0.1).castShadow = false;
  box(300, 0.04, STREET.roadZ1 - STREET.roadZ0, M(0x3b3f46), 0, -0.28, (STREET.roadZ0 + STREET.roadZ1) / 2).castShadow = false;
  const dash = M(0xe8e4d0);
  for (let x = -140; x <= 140; x += 6) box(2.6, 0.02, 0.14, dash, x, -0.25, (STREET.roadZ0 + STREET.roadZ1) / 2).castShadow = false;
  box(300, 0.04, 2.4, paving, 0, -0.3, STREET.roadZ1 + 1.2).castShadow = false;

  const glass = new THREE.MeshStandardMaterial({ color: 0xcfe9ff, transparent: true, opacity: 0.16, roughness: 0.05, metalness: 0.1, depthWrite: false });
  const frame = M(0xf7f5f0, { roughness: 0.5 });
  // Each shop keeps a warm light on at night: glowing ceiling panels plus one point light, both
  // driven by `glow.set(nightFactor)` (0 by day, 1 at night).
  const glow = { mats: [], lights: [], set(night) {
    for (const m of this.mats) m.emissiveIntensity = 0.1 + night * 1.7;
    for (const l of this.lights) l.intensity = night * 22;
  } };
  const sign = (text, color) => {
    const tex = art.signTexture(THREE, text, { width: 640, height: 160, background: color, color: "#ffffff", size: 78 });
    return new THREE.MeshBasicMaterial({ map: tex, transparent: true, toneMapped: false });
  };

  function shell(name, cx, wallColor, floorColor, signText, signColor) {
    const wall = M(wallColor), left = cx - WIDTH / 2, right = cx + WIDTH / 2;
    box(WIDTH, 0.12, Z1 - Z0, M(floorColor, { roughness: 0.6 }), cx, 0.04, (Z0 + Z1) / 2).castShadow = false;
    // Back wall, side walls, and the glass front: two panes either side of the door, a lintel above
    // the door gap, a frame, and a band on the back wall for the sign.
    box(WIDTH, WALL, 0.25, wall, cx, WALL / 2, Z0);
    box(0.25, WALL, Z1 - Z0, wall, left, WALL / 2, (Z0 + Z1) / 2);
    box(0.25, WALL, Z1 - Z0, wall, right, WALL / 2, (Z0 + Z1) / 2);
    const piece = (WIDTH - DOOR_W) / 2;
    for (const sx of [-1, 1]) {
      const pane = new THREE.Mesh(new THREE.PlaneGeometry(piece, WALL - 0.4), glass);
      pane.position.set(cx + sx * (DOOR_W + piece) / 2, 0.4 + (WALL - 0.4) / 2, Z1);
      group.add(pane);
      box(piece, 0.4, 0.2, frame, cx + sx * (DOOR_W + piece) / 2, 0.2, Z1);
    }
    box(DOOR_W, WALL - 2.4, 0.2, frame, cx, (WALL + 2.4) / 2, Z1);
    box(WIDTH, 0.12, 0.2, frame, cx, WALL, Z1);
    box(0.12, WALL, 0.2, frame, left, WALL / 2, Z1);
    box(0.12, WALL, 0.2, frame, right, WALL / 2, Z1);
    box(0.1, WALL, 0.2, frame, cx - DOOR_W / 2, WALL / 2, Z1);
    box(0.1, WALL, 0.2, frame, cx + DOOR_W / 2, WALL / 2, Z1);
    // The sign stands on top of the back wall, so it never hides the visitors.
    box(5.6, 1.5, 0.12, M(0x2b2e36), cx, WALL + 0.75, Z0 + 0.02);
    const board = new THREE.Mesh(new THREE.PlaneGeometry(5.4, 1.35), sign(signText, signColor));
    board.position.set(cx, WALL + 0.75, Z0 + 0.1);
    group.add(board);
    // Warm ceiling panels over the shop floor, and a point light under them.
    const panelM = M(0xfff0d0, { emissive: 0xffc878, emissiveIntensity: 0.1 });
    glow.mats.push(panelM);
    for (const pz of [7.4, 10.4]) box(2.6, 0.08, 0.7, panelM, cx, WALL - 0.06, pz).castShadow = false;
    const light = new THREE.PointLight(0xffc27a, 0, 11, 1.6);
    light.position.set(cx, WALL - 0.5, 9);
    group.add(light);
    glow.lights.push(light);
    // A door mat and planters either side of the door.
    box(1.5, 0.03, 0.9, M(0x5a3d2b), cx, 0.07, Z1 + 0.55).castShadow = false;
    for (const sx of [-1, 1]) {
      cyl(0.28, 0.22, 0.5, M(0xb5651d), cx + sx * 1.5, 0.25, Z1 + 0.5);
      const bush = new THREE.Mesh(new THREE.SphereGeometry(0.34, 10, 8), M(0x3d9f55, { roughness: 0.8 }));
      bush.position.set(cx + sx * 1.5, 0.75, Z1 + 0.5);
      bush.castShadow = true;
      group.add(bush);
    }
  }

  // ---- the coffee shop: a counter at the back with a machine and cups, two small tables ----
  {
    const cx = SHOPS.coffee.cx, cz = Z0 + 1.2;
    shell("coffee", cx, 0xd9b99b, 0x7a5a3a, strings.coffee_shop || "", "#6b3f23");
    box(5.4, 1.0, 0.7, M(0x5b3a24), cx, 0.55, cz);
    box(5.6, 0.08, 0.9, M(0xe8dcc8), cx, 1.08, cz);
    box(0.7, 0.55, 0.45, M(0xb8bcc4, { metalness: 0.5, roughness: 0.4 }), cx - 1.6, 1.4, cz);
    box(0.35, 0.3, 0.3, M(0x2a2d34), cx + 0.6, 1.27, cz);
    for (const dx of [-0.2, 0.2, 1.4, 1.9]) cyl(0.07, 0.06, 0.12, M(0xffffff), cx + dx, 1.18, cz + 0.15, 10);
    for (const tx of [-3, 3]) {
      cyl(0.5, 0.5, 0.05, M(0xe8dcc8), cx + tx, 0.85, 10.6);
      cyl(0.06, 0.06, 0.85, M(0x4a4f5a), cx + tx, 0.43, 10.6, 8);
      cyl(0.2, 0.2, 0.05, M(0x4a4f5a), cx + tx, 0.5, 9.9);
      cyl(0.2, 0.2, 0.05, M(0x4a4f5a), cx + tx, 0.5, 11.3);
    }
    // Menu board on the left wall.
    box(0.05, 1.0, 1.6, M(0x23262d), cx - WIDTH / 2 + 0.15, 2.0, 9.0);
  }

  // ---- the bookstore: shelves along both side walls, a display table and a till ----
  {
    const cx = SHOPS.books.cx;
    shell("books", cx, 0xb9c9d9, 0x8a6a48, strings.bookstore || "", "#2f4a6b");
    const palette = [0xc4574b, 0x4b79c4, 0xd1a84b, 0x4bc48a, 0x9a6ad1, 0xe8dcc8, 0x3d8f8f, 0xd17a4b];
    let i = 0;
    for (const sx of [-1, 1]) {
      const x = cx + sx * (WIDTH / 2 - 0.45);
      box(0.4, 2.3, 4.6, M(0x6b4a2b), x, 1.15, 8.6);
      for (let row = 0; row < 4; row++) {
        for (let k = 0; k < 11; k++) {
          const h = 0.32 + ((k * 7 + row * 3) % 5) * 0.03;
          box(0.3, h, 0.28, M(palette[(i++) % palette.length]), x - sx * 0.1, 0.35 + row * 0.55 + h / 2, 6.7 + k * 0.38, group).castShadow = false;
        }
      }
    }
    box(2.4, 0.8, 1.2, M(0x8a6a48), cx, 0.4, 8.4);
    for (let k = 0; k < 6; k++) box(0.5, 0.06, 0.38, M(palette[(k * 3) % palette.length]), cx - 0.8 + (k % 3) * 0.8, 0.84 + Math.floor(k / 3) * 0.07, 8.2 + (k % 2) * 0.35).rotation.y = k * 0.4;
    // The till counter at the back, and a reading armchair by the glass.
    box(2.4, 1.0, 0.6, M(0x6b4a2b), cx, 0.55, Z0 + 1.2);
    box(2.5, 0.06, 0.7, M(0xe8dcc8), cx, 1.08, Z0 + 1.2);
    box(0.5, 0.35, 0.3, M(0x2a2d34), cx + 0.7, 1.27, Z0 + 1.2);
    box(0.9, 0.5, 0.9, M(0x9b3d3d), cx - 2.4, 0.3, 11.0);
    box(0.9, 0.8, 0.2, M(0x9b3d3d), cx - 2.4, 0.7, 11.4);
  }
  return { group, glow };
}
