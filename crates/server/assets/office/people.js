// The office people, ported from the prototype's `person`: a blocky figure whose
// body, head, arms and legs are separate groups so the agents can pose them. Each
// loop's look is a stable function of its name.

const SHIRTS = [0x4b79c4, 0xc4574b, 0x4bc48a, 0x9a6ad1, 0xd1a84b, 0x4bb5c4, 0xc44b8f, 0x6a8f3d, 0xd17a4b, 0x5b6ad1, 0x3d8f8f];
const SKINS = [0xe0b48f, 0xc68e66, 0x8d5a3b, 0xf1c9a5, 0xa8714d, 0xe8bf9a];
const HAIRS = [0x2b1d14, 0x6b4a2b, 0x111111, 0xb5651d];
const PANTS = [0x2c3140, 0x3b3b3b, 0x4a3f35, 0x23324a];

/// 32-bit FNV-1a of `text`, salted so each body part gets its own draw.
function hash(text, salt) {
  let h = (0x811c9dc5 ^ salt) >>> 0;
  for (const ch of text) {
    h ^= ch.codePointAt(0);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h;
}

/// The same name always gets the same shirt, skin, hair and trousers.
export function appearanceFor(name) {
  const s = String(name);
  const pick = (list, salt) => list[hash(s, salt) % list.length];
  return { shirt: pick(SHIRTS, 1), skin: pick(SKINS, 2), hair: pick(HAIRS, 3), pants: pick(PANTS, 4) };
}

// Geometries are shared by every person (per THREE instance).
const geometryCache = new WeakMap();
function geometries(THREE) {
  let g = geometryCache.get(THREE);
  if (!g) {
    const shin = new THREE.CylinderGeometry(0.09, 0.08, 0.55, 10);
    g = {
      torso: new THREE.BoxGeometry(0.62, 0.8, 0.36),
      head: new THREE.SphereGeometry(0.28, 24, 18),
      hair: new THREE.SphereGeometry(0.3, 24, 18, 0, Math.PI * 2, 0, Math.PI * 0.55),
      eye: new THREE.SphereGeometry(0.035, 8, 8),
      arm: new THREE.CylinderGeometry(0.09, 0.08, 0.8, 10),
      hand: new THREE.SphereGeometry(0.09, 10, 8),
      thigh: new THREE.CylinderGeometry(0.1, 0.09, 0.42, 10),
      shin,
      shoe: new THREE.BoxGeometry(0.16, 0.08, 0.26),
      mug: new THREE.CylinderGeometry(0.07, 0.06, 0.14, 12),
    };
    geometryCache.set(THREE, g);
  }
  return g;
}

/// A person facing -z, feet at the origin. `shirtMat` is recoloured grey when the
/// loop is disabled; `mug` (in the right hand) starts hidden.
export function buildPerson(THREE, look) {
  const G = geometries(THREE);
  const M = (c) => new THREE.MeshStandardMaterial({ color: c, roughness: 0.7, metalness: 0.05 });
  const mesh = (geo, mat, x, y, z, parent) => {
    const m = new THREE.Mesh(geo, mat);
    m.position.set(x, y, z);
    m.castShadow = true;
    m.receiveShadow = true;
    parent.add(m);
    return m;
  };
  const group = new THREE.Group();
  const body = new THREE.Group();
  group.add(body);
  const shirtMat = M(look.shirt), skinMat = M(look.skin), pantsMat = M(look.pants);
  mesh(G.torso, shirtMat, 0, 1.4, 0, body);
  const head = new THREE.Group();
  head.position.set(0, 2.1, 0);
  body.add(head);
  mesh(G.head, skinMat, 0, 0, 0, head);
  mesh(G.hair, M(look.hair), 0, 0.04, 0.03, head);
  const eyeMat = M(0x111111);
  for (const ex of [-0.1, 0.1]) mesh(G.eye, eyeMat, ex, 0.02, -0.26, head).castShadow = false;
  const arm = (sx) => {
    const a = new THREE.Group();
    a.position.set(sx * 0.4, 1.7, 0);
    body.add(a);
    mesh(G.arm, shirtMat, 0, -0.4, 0, a);
    mesh(G.hand, skinMat, 0, -0.84, 0, a);
    return a;
  };
  const shoeMat = M(0x1a1a1a);
  const leg = (sx) => {
    const hip = new THREE.Group();
    hip.position.set(sx * 0.16, 1.0, 0);
    body.add(hip);
    mesh(G.thigh, pantsMat, 0, -0.21, 0, hip);
    const knee = new THREE.Group();
    knee.position.set(0, -0.42, 0);
    hip.add(knee);
    mesh(G.shin, pantsMat, 0, -0.275, 0, knee);
    mesh(G.shoe, shoeMat, 0, -0.58, -0.06, knee);
    return { hip, knee };
  };
  const lArm = arm(-1), rArm = arm(1);
  const mug = mesh(G.mug, M(0xffffff), 0, -0.9, -0.08, rArm);
  mug.visible = false;
  return { group, body, head, lArm, rArm, legL: leg(-1), legR: leg(1), mug, shirtMat };
}
