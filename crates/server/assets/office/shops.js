// The two shops on the plaza in front of the office: a coffee shop (left) and a bookstore
// (right). Each is a roofless box with a glass front towards the viewer, so the visitors
// inside can be seen, and a door facing the office. `SHOPS` is plain data (where the door
// is and where visitors stand); `buildShops` draws them into the scene.

/// Shop footprint: `Z0` is the door side (towards the office), `Z1` the glass front.
const Z0 = 17, Z1 = 22.5, WIDTH = 8, WALL = 3.4, DOOR_W = 1.7;

/// For each shop: its centre `cx`, the door point, and the spots visitors stand at, each
/// facing something (`face`): the counter for coffee, the shelves and display for books.
export const SHOPS = {
  coffee: {
    cx: -9,
    spots: [[-10.6, 20.2, -10.6, 21.4], [-9, 20.2, -9, 21.4], [-7.4, 20.2, -7.4, 21.4]],
  },
  books: {
    cx: 9,
    spots: [[6.7, 19.4, 5.4, 19.4], [11.3, 19.4, 12.6, 19.4], [9, 21, 9, 19.5]],
  },
};
for (const s of Object.values(SHOPS)) {
  s.door = [s.cx, Z0];
  s.spots = s.spots.map(([x, z, fx, fz]) => ({ x, z, face: [fx, fz] }));
}
/// The plaza's row of lane points between the office's front edge and the doors.
export const PLAZA_Z = 14.6;
export const DOOR_APPROACH_Z = 15.8;

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

  // The plaza: paving from the office's front edge past both doors, kerb and a few planters.
  const paving = M(0xcfcfd2, { roughness: 0.9 });
  box(40, 0.06, 4.9, paving, 0, 0.0, 15.0).castShadow = false;
  box(40, 0.1, 0.12, M(0xa9a9ae), 0, 0.05, 12.55).castShadow = false;

  const glass = new THREE.MeshStandardMaterial({ color: 0xcfe9ff, transparent: true, opacity: 0.16, roughness: 0.05, metalness: 0.1, depthWrite: false });
  const frame = M(0xf7f5f0, { roughness: 0.5 });
  const sign = (text, color) => {
    const tex = art.signTexture(THREE, text, { width: 640, height: 160, background: color, color: "#ffffff", size: 78 });
    return new THREE.MeshBasicMaterial({ map: tex, transparent: true, toneMapped: false });
  };

  function shell(name, cx, wallColor, floorColor, signText, signColor) {
    const wall = M(wallColor), left = cx - WIDTH / 2, right = cx + WIDTH / 2;
    box(WIDTH, 0.12, Z1 - Z0, M(floorColor, { roughness: 0.6 }), cx, 0.04, (Z0 + Z1) / 2).castShadow = false;
    // Door side: two wall pieces and a lintel above the door gap.
    const piece = (WIDTH - DOOR_W) / 2;
    box(piece, WALL, 0.25, wall, left + piece / 2, WALL / 2, Z0);
    box(piece, WALL, 0.25, wall, right - piece / 2, WALL / 2, Z0);
    box(DOOR_W, WALL - 2.4, 0.25, wall, cx, (WALL + 2.4) / 2, Z0);
    // Side walls, and the glass front with a frame and a band for the sign above it.
    box(0.25, WALL, Z1 - Z0, wall, left, WALL / 2, (Z0 + Z1) / 2);
    box(0.25, WALL, Z1 - Z0, wall, right, WALL / 2, (Z0 + Z1) / 2);
    const front = new THREE.Mesh(new THREE.PlaneGeometry(WIDTH, WALL - 0.4), glass);
    front.position.set(cx, 0.4 + (WALL - 0.4) / 2, Z1);
    group.add(front);
    box(WIDTH, 0.4, 0.2, frame, cx, 0.2, Z1);
    box(WIDTH, 0.12, 0.2, frame, cx, WALL, Z1);
    box(0.12, WALL, 0.2, frame, left, WALL / 2, Z1);
    box(0.12, WALL, 0.2, frame, right, WALL / 2, Z1);
    // The sign stands on top of the door-side wall, behind the interior, so it never hides the visitors.
    box(5.6, 1.5, 0.12, M(0x2b2e36), cx, WALL + 0.75, Z0 - 0.02);
    const board = new THREE.Mesh(new THREE.PlaneGeometry(5.4, 1.35), sign(signText, signColor));
    board.position.set(cx, WALL + 0.75, Z0 + 0.05);
    group.add(board);
    // A door mat and planters either side of the door.
    box(1.5, 0.03, 0.9, M(0x5a3d2b), cx, 0.07, Z0 - 0.55).castShadow = false;
    for (const sx of [-1, 1]) {
      cyl(0.28, 0.22, 0.5, M(0xb5651d), cx + sx * 1.5, 0.25, Z0 - 0.5);
      const bush = new THREE.Mesh(new THREE.SphereGeometry(0.34, 10, 8), M(0x3d9f55, { roughness: 0.8 }));
      bush.position.set(cx + sx * 1.5, 0.75, Z0 - 0.5);
      bush.castShadow = true;
      group.add(bush);
    }
  }

  // ---- the coffee shop: a counter with a machine and cups, two small tables ----
  {
    const cx = SHOPS.coffee.cx;
    shell("coffee", cx, 0xd9b99b, 0x7a5a3a, strings.coffee_shop || "", "#6b3f23");
    box(5.4, 1.0, 0.7, M(0x5b3a24), cx, 0.55, 21.5);
    box(5.6, 0.08, 0.9, M(0xe8dcc8), cx, 1.08, 21.5);
    box(0.7, 0.55, 0.45, M(0xb8bcc4, { metalness: 0.5, roughness: 0.4 }), cx - 1.6, 1.4, 21.5);
    box(0.35, 0.3, 0.3, M(0x2a2d34), cx + 0.6, 1.27, 21.5);
    for (const dx of [-0.2, 0.2, 1.4, 1.9]) cyl(0.07, 0.06, 0.12, M(0xffffff), cx + dx, 1.18, 21.35, 10);
    for (const tx of [-3, 3]) {
      cyl(0.5, 0.5, 0.05, M(0xe8dcc8), cx + tx, 0.85, 19.3);
      cyl(0.06, 0.06, 0.85, M(0x4a4f5a), cx + tx, 0.43, 19.3, 8);
      cyl(0.2, 0.2, 0.05, M(0x4a4f5a), cx + tx, 0.5, 18.6);
      cyl(0.2, 0.2, 0.05, M(0x4a4f5a), cx + tx, 0.5, 20.0);
    }
    // Menu board on the left wall.
    box(0.05, 1.0, 1.6, M(0x23262d), cx - WIDTH / 2 + 0.15, 2.0, 20.2);
  }

  // ---- the bookstore: shelves of books along both side walls, a display table in the middle ----
  {
    const cx = SHOPS.books.cx;
    shell("books", cx, 0xb9c9d9, 0x8a6a48, strings.bookstore || "", "#2f4a6b");
    const palette = [0xc4574b, 0x4b79c4, 0xd1a84b, 0x4bc48a, 0x9a6ad1, 0xe8dcc8, 0x3d8f8f, 0xd17a4b];
    let i = 0;
    for (const sx of [-1, 1]) {
      const x = cx + sx * (WIDTH / 2 - 0.45);
      box(0.4, 2.3, 4.6, M(0x6b4a2b), x, 1.15, 19.7);
      for (let row = 0; row < 4; row++) {
        for (let k = 0; k < 11; k++) {
          const h = 0.32 + ((k * 7 + row * 3) % 5) * 0.03;
          box(0.3, h, 0.28, M(palette[(i++) % palette.length]), x - sx * 0.1, 0.35 + row * 0.55 + h / 2, 17.8 + k * 0.38, group).castShadow = false;
        }
      }
    }
    box(2.4, 0.8, 1.2, M(0x8a6a48), cx, 0.4, 19.5);
    for (let k = 0; k < 6; k++) box(0.5, 0.06, 0.38, M(palette[(k * 3) % palette.length]), cx - 0.8 + (k % 3) * 0.8, 0.84 + Math.floor(k / 3) * 0.07, 19.3 + (k % 2) * 0.35).rotation.y = k * 0.4;
    // A reading armchair by the glass.
    box(0.9, 0.5, 0.9, M(0x9b3d3d), cx - 2.4, 0.3, 21.6);
    box(0.9, 0.8, 0.2, M(0x9b3d3d), cx - 2.4, 0.7, 22.0);
  }
  return group;
}
