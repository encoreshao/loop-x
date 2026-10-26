// The people of the street: a staff member behind each shop's counter, customers who walk in
// from the sidewalk, stand at the counter or the shelves for a while and leave again, and
// pedestrians who just pass by. All of them are `buildPerson` figures (people.js) driven by
// small waypoint walks; none of them is a colleague, so they never show up in the panel.

const V = new URL(import.meta.url).search;
const [{ buildPerson, appearanceFor }, { SHOPS, STREET }] = await Promise.all([import("./people.js" + V), import("./shops.js" + V)]);

const WALK_SPEED = 1.7;
/// Where the walkers appear and disappear, either side of the row.
const EDGE = 62;
const STAY_S = [10, 22], WAIT_S = [3, 14];
const angDiff = (a, b) => {
  let d = b - a;
  while (d > Math.PI) d -= 2 * Math.PI;
  while (d < -Math.PI) d += 2 * Math.PI;
  return d;
};
const yawTo = (dx, dz) => Math.atan2(-dx, -dz);

/// Builds the street's people into `scene`. `R` is the world's seeded random; `still` freezes
/// the motion (reduced motion). Returns `{folk, tick(dt, t)}`; `folk` are the moving groups.
export function buildTown(scene, THREE, { R, still }) {
  const folk = [];
  const between = ([lo, hi]) => lo + R() * (hi - lo);
  const make = (name) => {
    const p = buildPerson(THREE, appearanceFor(name));
    scene.add(p.group);
    return p;
  };

  // Staff: stand behind the counter facing the shop, arms resting on it, shifting a little.
  const staff = Object.entries(SHOPS).map(([place, shop], i) => {
    const p = make("staff-" + place);
    p.group.position.set(shop.staff.x, 0.04, shop.staff.z);
    p.group.rotation.y = Math.PI;
    folk.push({ g: p.group });
    return { p, phase: i * 2.1, place };
  });

  // The walkers. A customer has a shop to visit; a pedestrian walks the sidewalk end to end.
  const walkers = [];
  const lane = () => STREET.walkZ + (R() - 0.5) * 1.6;
  const spawn = (w) => {
    const dir = R() < 0.5 ? 1 : -1;
    w.x = -dir * EDGE;
    w.z = lane();
    w.dir = dir;
    w.speed = WALK_SPEED * (0.8 + R() * 0.5);
    w.path = [[dir * EDGE, w.z]];
    w.phase = "walk";
    w.wait = 0;
  };
  const pedestrians = 4, customersPerShop = 2;
  const entries = [];
  for (let i = 0; i < pedestrians; i++) entries.push({ name: "walker-" + i });
  for (const place of Object.keys(SHOPS)) for (let i = 0; i < customersPerShop; i++) entries.push({ name: `${place}-guest-${i}`, place });
  entries.forEach((e, i) => {
    const p = make(e.name);
    const w = { ...e, p, yaw: 0, g: p.group };
    spawn(w);
    // Spread them along the street (and the customers over time) so they do not start in a queue.
    w.x = (R() - 0.5) * 2 * EDGE;
    if (w.place) {
      w.wait = i * 2 + R() * 4;
      w.phase = "idle";
      w.path = [];
    }
    p.group.position.set(w.x, 0.04, w.z);
    p.group.visible = !w.place;
    folk.push({ g: p.group });
    walkers.push(w);
  });
  const taken = new Map(); // guest spot -> customer using it

  function startVisit(w) {
    const shop = SHOPS[w.place], spot = shop.guests.find((s) => !taken.has(s));
    if (!spot) {
      w.wait = between(WAIT_S);
      return;
    }
    taken.set(spot, w);
    w.spot = spot;
    const from = shop.cx + (R() < 0.5 ? -1 : 1) * (14 + R() * 20);
    w.x = Math.max(-EDGE, Math.min(EDGE, from));
    w.z = lane();
    w.speed = WALK_SPEED * (0.8 + R() * 0.4);
    const dz = shop.door[1];
    w.path = [[shop.cx, w.z], [shop.cx, STREET.doorApproachZ], [shop.cx, dz - 0.3], [shop.cx, dz - 1.4], [spot.x, spot.z]];
    w.phase = "in";
    w.g.position.set(w.x, 0.04, w.z);
    w.g.visible = true;
  }
  function leave(w) {
    const shop = SHOPS[w.place];
    taken.delete(w.spot);
    w.spot = null;
    const dir = R() < 0.5 ? 1 : -1;
    w.z = lane();
    w.path = [[shop.cx, shop.door[1] - 1.4], [shop.cx, shop.door[1] - 0.3], [shop.cx, STREET.doorApproachZ], [shop.cx, w.z], [dir * EDGE, w.z]];
    w.phase = "out";
  }

  function stepWalker(w, dt, t) {
    const p = w.g.position;
    let walking = false;
    if (w.path.length) {
      const [tx, tz] = w.path[0], dx = tx - p.x, dz = tz - p.z, d = Math.hypot(dx, dz), step = w.speed * dt;
      if (d <= step) {
        p.x = tx;
        p.z = tz;
        w.path.shift();
      } else {
        p.x += (dx / d) * step;
        p.z += (dz / d) * step;
        w.yawT = yawTo(dx, dz);
        walking = true;
      }
    }
    if (!w.path.length) {
      if (w.phase === "in") {
        w.phase = "stay";
        w.stay = between(STAY_S);
      } else if (w.phase === "out" || w.phase === "walk") {
        if (w.place) {
          w.g.visible = false;
          w.phase = "idle";
          w.wait = between(WAIT_S);
        } else {
          spawn(w);
          p.set(w.x, 0.04, w.z);
        }
      } else if (w.phase === "stay") {
        if ((w.stay -= dt) <= 0) leave(w);
        else w.yawT = yawTo(w.spot.face[0] - p.x, w.spot.face[1] - p.z);
      } else if (w.phase === "idle" && (w.wait -= dt) <= 0) startVisit(w);
    }
    w.yaw += angDiff(w.yaw, w.yawT ?? w.yaw) * Math.min(1, dt * 8);
    w.g.rotation.y = w.yaw;
    const s = walking ? Math.sin(t * 6 * w.speed + w.x) * 0.6 : 0, P = w.p;
    P.legL.hip.rotation.x = s;
    P.legR.hip.rotation.x = -s;
    P.legL.knee.rotation.x = walking ? Math.max(0, -s) * 0.8 : 0;
    P.legR.knee.rotation.x = walking ? Math.max(0, s) * 0.8 : 0;
    const holding = w.phase === "stay" && w.place === "books";
    P.lArm.rotation.x = walking ? -s * 0.8 : holding ? 1.2 : w.phase === "stay" ? 0.6 : 0.15;
    P.rArm.rotation.x = walking ? s * 0.8 : holding ? 1.2 : w.phase === "stay" ? 1.1 + (Math.sin(t * 0.9 + w.x) > 0.8 ? 1.1 : 0) : 0.15;
    P.head.rotation.x = holding ? 0.4 : 0;
  }

  function tick(dt, t) {
    if (still) return;
    for (const s of staff) {
      const k = t + s.phase;
      s.p.lArm.rotation.x = 0.9 + Math.sin(k * 1.3) * 0.12;
      s.p.rArm.rotation.x = s.place === "coffee" ? 1.0 + Math.sin(k * 2.2) * 0.35 : 0.9 + Math.sin(k * 0.8) * 0.1;
      s.p.head.rotation.y = Math.sin(k * 0.5) * 0.3;
    }
    for (const w of walkers) stepWalker(w, dt, t);
  }
  // Staff pose once so that a still (reduced-motion) scene is not left with stiff arms.
  for (const s of staff) {
    s.p.lArm.rotation.x = 0.9;
    s.p.rArm.rotation.x = 0.9;
  }
  return { folk, tick };
}
