// One person per loop, placed and moved by the loop's office state. Ported from the
// prototype's agents: `plan`/`leavePts`/`enterPts`/`arrive` route people along the
// aisles and the central corridor, `updateAgent` walks, sits and poses them, and
// each wears a name tag and a mug/glyph bubble (canvas sprites, so a loop name is
// always text, never markup). Working people at their seat get a faint data beam
// to the AI centre rack; disabled people wear grey.

const V = new URL(import.meta.url).search;
const [{ HUB, COR, rng, freezeStatic }, { buildLayout, furnish, MAX_DRAWN }, { appearanceFor, buildPerson }] = await Promise.all([
  import("./world.js" + V),
  import("./layout.js" + V),
  import("./people.js" + V),
]);

const COLORS = { working: 0x3ddc84, waiting: 0xffb020, idle: 0x7d8590, failed: 0xff5252, disabled: 0x6b7280 };
const GLYPHS = { working: "⌨️", waiting: "✋", failed: "⚠️", disabled: "💤" };
const DISABLED_SHIRT = 0x7b808a;
const WALK_SPEED = 2.6;
/// The front aisle between table B and the "Needs you" mat, and where it meets the
/// walk along the standby bench.
const FRONT_Z = 8, BENCH_WALK_X = -12.8, WAIT_Z = 9.9;

const lerp = (a, b, t) => a + (b - a) * t;
function angDiff(a, b) {
  let d = b - a;
  while (d > Math.PI) d -= 2 * Math.PI;
  while (d < -Math.PI) d += 2 * Math.PI;
  return d;
}
/// A stable phase in [0, 10) from the name, so poses do not depend on load time.
function phaseOf(name) {
  let h = 0;
  for (const ch of name) h = (Math.imul(h, 31) + ch.codePointAt(0)) | 0;
  return ((h >>> 0) % 1000) / 100;
}

/// `ctx`: {THREE, scene, handles (world handles: `aiTop`, `aiRate`), strings
/// (`#office-i18n`), reducedMotion}. Under reduced motion nobody walks or wanders:
/// a state change moves the person straight to their new place, and poses hold still.
export function createAgents(ctx) {
  const { THREE, scene, handles } = ctx;
  const strings = ctx.strings || {};
  const still = !!ctx.reducedMotion;
  // Seeded, so the same states give the same spots on every load.
  const R = rng(5);
  const agents = new Map();
  let order = [];
  const scratch = new THREE.Vector3();
  let layout = null, furniture = null, labelScale = 1, overflow = 0;

  // ---------- sprites ----------
  function sprite(w, h, cw, ch) {
    const c = document.createElement("canvas");
    c.width = cw;
    c.height = ch;
    const tex = new THREE.CanvasTexture(c);
    tex.colorSpace = THREE.SRGBColorSpace;
    // Depth-tested so tags and bubbles never show through walls or furniture.
    const s = new THREE.Sprite(new THREE.SpriteMaterial({ map: tex, transparent: true, depthWrite: false, depthTest: true }));
    s.scale.set(w, h, 1);
    s.renderOrder = 10;
    s.userData = { c, tex, w, h };
    return s;
  }
  /// The name tag grows a little (and turns fully opaque) while hovered or selected.
  function scaleLabel(a) {
    const k = a.hi ? 1.18 : 1;
    a.label.scale.set(2.8 * labelScale * k, 0.875 * labelScale * k, 1);
    a.label.material.opacity = a.hi ? 1 : 0.92;
  }
  function drawLabel(a) {
    const { c, tex } = a.label.userData, x = c.getContext("2d");
    x.clearRect(0, 0, c.width, c.height);
    x.fillStyle = "rgba(18,20,26,.88)";
    x.beginPath();
    x.roundRect(4, 4, c.width - 8, c.height - 8, 28);
    x.fill();
    x.fillStyle = "#fff";
    x.textAlign = "center";
    const max = c.width - 40;
    let px = 54, name = a.title || a.name;
    const font = (n, w) => w + " " + n + "px -apple-system,system-ui,sans-serif";
    x.font = font(px, 600);
    while (px > 30 && x.measureText(name).width > max) x.font = font((px -= 2), 600);
    if (x.measureText(name).width > max) {
      while (name.length > 1 && x.measureText(name + "…").width > max) name = name.slice(0, -1);
      name += "…";
    }
    x.fillText(name, c.width / 2, 70);
    const text = strings["state_" + a.state] || "";
    x.font = font(40, 500);
    const tw = Math.min(x.measureText(text).width, max - 40);
    x.fillStyle = "#" + COLORS[a.state].toString(16).padStart(6, "0");
    x.beginPath();
    x.arc(c.width / 2 - tw / 2 - 26, 116, 12, 0, 7);
    x.fill();
    x.fillText(text, c.width / 2 + 8, 130, max - 40);
    tex.needsUpdate = true;
  }
  function setGlyph(a, glyph) {
    if (a.glyph === glyph) return;
    a.glyph = glyph;
    const { c, tex } = a.bubble.userData, x = c.getContext("2d");
    x.clearRect(0, 0, c.width, c.height);
    if (glyph) {
      x.fillStyle = "rgba(255,255,255,.95)";
      x.beginPath();
      x.arc(64, 56, 48, 0, 7);
      x.fill();
      x.beginPath();
      x.moveTo(50, 98);
      x.lineTo(64, 122);
      x.lineTo(78, 98);
      x.fill();
      x.font = "56px serif";
      x.textAlign = "center";
      x.textBaseline = "middle";
      x.fillText(glyph, 64, 58);
    }
    tex.needsUpdate = true;
  }

  // ---------- people ----------
  function create(name) {
    const p = buildPerson(THREE, appearanceFor(name));
    p.group.userData.agent = name;
    scene.add(p.group);
    const label = sprite(2.8 * labelScale, 0.875 * labelScale, 512, 160);
    label.position.set(0, 3.3, 0);
    label.material.opacity = 0.92;
    p.group.add(label);
    const bubble = sprite(0.8, 0.8, 128, 128);
    bubble.position.set(0, 2.6, 0);
    p.group.add(bubble);
    const beam = new THREE.Line(
      new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(), new THREE.Vector3()]),
      new THREE.LineBasicMaterial({ color: 0x6aa8ff, transparent: true, opacity: 0 })
    );
    beam.frustumCulled = false;
    beam.visible = false;
    scene.add(beam);
    return {
      name, state: null, info: null, ...p, label, bubble, beam,
      desk: null, loc: null, dest: null, path: [], sit: 0, sitY: 0, sitOn: false,
      yaw: 0, yawT: 0, t0: phaseOf(name), wander: 8 + R() * 10, glyph: null, faceT: null,
    };
  }
  function dispose(a) {
    free(held(a), a);
    scene.remove(a.group);
    scene.remove(a.beam);
    a.beam.geometry.dispose();
    a.beam.material.dispose();
    // Person geometries are shared; materials and the sprite textures are not.
    a.group.traverse((o) => {
      if (!o.material) return;
      if (o.material.map) o.material.map.dispose();
      o.material.dispose();
    });
  }
  function setState(a, s) {
    a.state = s;
    drawLabel(a);
    // An idle person's bubble is drawn by the lounge pose; drop the previous state's glyph.
    setGlyph(a, s === "idle" ? null : GLYPHS[s]);
    a.mug.visible = s === "idle";
    a.shirtMat.color.setHex(s === "disabled" ? DISABLED_SHIRT : appearanceFor(a.name).shirt);
  }

  // ---------- places ----------
  const held = (a) => (a.path.length ? a.dest : a.loc);
  function free(place, a) {
    if (place && place.spot && place.spot.occ === a) place.spot.occ = null;
  }
  /// A free spot: any in the lounge, the first free one (bench and mat fill up from
  /// the front) elsewhere.
  function pick(list, anywhere) {
    const open = list.filter((s) => !s.occ);
    return open.length ? open[anywhere ? Math.floor(R() * open.length) : 0] : null;
  }
  const wantOf = (a) => (a.state === "disabled" ? "off" : a.state === "waiting" ? "wait" : a.state === "idle" ? "lounge" : "desk");
  /// Where `a` should go now; waiting and disabled people fall back to the lounge
  /// when their zone is full.
  function target(a, notSpot) {
    let kind = wantOf(a);
    if (kind === "desk") return { kind, desk: a.desk };
    if (kind === "wait" || kind === "off") {
      const spot = pick(kind === "wait" ? layout.wait : layout.off);
      if (spot) return { kind, spot };
      kind = "lounge";
    }
    const spot = pick(layout.lounge.filter((s) => s !== notSpot), true);
    return spot ? { kind: "lounge", spot } : null;
  }
  function needsMove(a) {
    const want = wantOf(a);
    if (want === "desk") return a.loc.kind !== "desk" || a.loc.desk !== a.desk;
    if (want === a.loc.kind) return false;
    // Already in the lounge because the zone was full: stay until a spot frees up.
    if (a.loc.kind === "lounge" && want === "wait" && !layout.wait.some((s) => !s.occ)) return false;
    if (a.loc.kind === "lounge" && want === "off" && !layout.off.some((s) => !s.occ)) return false;
    return true;
  }
  const benchRoute = (spot) => (spot.via.length ? spot.via : [[BENCH_WALK_X, FRONT_Z], [BENCH_WALK_X, spot.z]]);
  // Route planning: seat <-> corridor <-> lounge / "Needs you" / standby.
  function leavePts(loc, nextLounge) {
    if (loc.kind === "desk") {
      const d = loc.desk;
      return [[d.x + d.gap, d.z], [d.x + d.gap, d.za], [0, d.za], COR];
    }
    if (loc.kind === "off") return [...[...benchRoute(loc.spot)].reverse(), [0, FRONT_Z], COR];
    if (loc.kind === "lounge") {
      const v = [...loc.spot.via].reverse();
      return nextLounge ? [...v, HUB] : [...v, HUB, COR];
    }
    return [[loc.spot.x, WAIT_Z], [0, WAIT_Z], COR];
  }
  function enterPts(dest, fromLounge) {
    if (dest.kind === "desk") {
      const d = dest.desk;
      return [[0, d.za], [d.x + d.gap, d.za], [d.x + d.gap, d.z], [d.x, d.z]];
    }
    const s = dest.spot;
    if (dest.kind === "off") return [[0, FRONT_Z], ...benchRoute(s), [s.x, s.z]];
    if (dest.kind === "lounge") return [...(fromLounge ? [] : [HUB]), ...s.via, [s.x, s.z]];
    return [[0, WAIT_Z], [s.x, WAIT_Z], [s.x, s.z]];
  }
  const yawTo = (a, t) => Math.atan2(-(t[0] - a.group.position.x), -(t[1] - a.group.position.z));
  /// Settles `a` at `loc`: what to face and whether to sit.
  function settle(a, loc) {
    a.loc = loc;
    a.dest = null;
    if (loc.kind === "desk") {
      a.faceT = [loc.desk.x, loc.desk.zc];
      a.sitY = -0.37;
      a.sitOn = true;
    } else {
      a.faceT = loc.spot.face;
      a.sitY = loc.spot.sitY || 0;
      a.sitOn = !!loc.spot.sitY;
    }
    a.wander = 8 + R() * 12;
  }
  /// Puts `a` straight at its place, no walking (first sight, new layout, reduced motion).
  function place(a) {
    free(held(a), a);
    a.path = [];
    a.loc = null;
    let dest = target(a, null);
    // Every spot taken (only possible with a full room): share a lounge spot rather than vanish.
    if (!dest) dest = { kind: "lounge", spot: layout.lounge[(a.idx || 0) % layout.lounge.length] };
    else if (dest.spot) dest.spot.occ = a;
    const p = dest.kind === "desk" ? dest.desk : dest.spot;
    a.group.position.set(p.x, 0, p.z);
    settle(a, dest);
    a.sit = a.sitOn ? 1 : 0;
    a.yaw = a.yawT = yawTo(a, a.faceT);
    a.group.rotation.y = a.yaw;
  }
  /// After a new layout: someone standing still on a spot that exists at the same place
  /// in it stays put (rebound to the new spot); everyone else is placed afresh.
  function carryOver(a) {
    const old = a.loc;
    a.dest = null;
    if (old && !a.path.length) {
      const same = (p) => Math.abs(p.x - old[old.kind === "desk" ? "desk" : "spot"].x) < 1e-6 && Math.abs(p.z - old[old.kind === "desk" ? "desk" : "spot"].z) < 1e-6;
      if (old.kind === "desk") {
        if (same(a.desk)) {
          a.loc = { kind: "desk", desk: a.desk };
          return;
        }
      } else {
        const spot = layout[old.kind].find((s) => !s.occ && same(s));
        if (spot) {
          spot.occ = a;
          a.loc = { kind: old.kind, spot };
          return;
        }
      }
    }
    a.loc = null;
    a.path = [];
  }
  function plan(a) {
    const dest = target(a, a.loc.kind === "lounge" ? a.loc.spot : null);
    if (!dest) return false;
    if (dest.kind === "desk" && a.loc.kind === "desk" && a.loc.desk === dest.desk) return false;
    const bothLounge = dest.kind === "lounge" && a.loc.kind === "lounge";
    free(a.loc, a);
    if (dest.spot) dest.spot.occ = a;
    a.path = [...leavePts(a.loc, bothLounge), ...enterPts(dest, bothLounge)];
    a.dest = dest;
    return true;
  }

  // ---------- animation ----------
  function update(a, dt, t) {
    if (!a.loc) return;
    const k = still ? a.t0 : t + a.t0;
    let walking = false;
    if (!a.path.length) {
      // Idle people wander between lounge spots now and then.
      if (!still && a.state === "idle" && a.loc.kind === "lounge") {
        a.wander -= dt;
        // No free spot to wander to: wait a while before looking again.
        if (a.wander < 0 && !plan(a)) a.wander = 8 + R() * 12;
      }
      // A pending change of place (also a change that arrived mid-walk).
      if (!a.path.length && needsMove(a)) {
        if (still) place(a);
        else plan(a);
      }
    }
    const wantSit = !a.path.length && a.sitOn;
    a.sit = still ? (wantSit ? 1 : 0) : lerp(a.sit, wantSit ? 1 : 0, Math.min(1, dt * 6));
    const p = a.group.position;
    if (a.path.length && a.sit < 0.15) {
      const [tx, tz] = a.path[0];
      const dx = tx - p.x, dz = tz - p.z, d = Math.hypot(dx, dz), step = WALK_SPEED * dt;
      if (d <= step) {
        p.x = tx;
        p.z = tz;
        a.path.shift();
        if (!a.path.length) settle(a, a.dest);
      } else {
        p.x += (dx / d) * step;
        p.z += (dz / d) * step;
        a.yawT = Math.atan2(-dx, -dz);
        walking = true;
      }
    } else if (!a.path.length && a.faceT) {
      a.yawT = yawTo(a, a.faceT);
    }
    a.yaw = still ? a.yawT : a.yaw + angDiff(a.yaw, a.yawT) * Math.min(1, dt * 8);
    a.group.rotation.y = a.yaw;

    const sit = a.sit;
    a.body.position.y = lerp(0, a.sitY, sit);
    const sw = walking ? Math.sin(t * 9) * 0.6 : 0;
    a.legL.hip.rotation.x = lerp(sw, 1.5, sit);
    a.legR.hip.rotation.x = lerp(-sw, 1.5, sit);
    a.legL.knee.rotation.x = lerp(walking ? Math.max(0, -sw) * 0.8 : 0, -1.5, sit);
    a.legR.knee.rotation.x = lerp(walking ? Math.max(0, sw) * 0.8 : 0, -1.5, sit);
    let lx = 0.15, rx = 0.15, lz = 0, rz = 0, hx = 0;
    a.body.position.x = 0;
    const atDesk = a.loc.kind === "desk" && !a.path.length;
    if (walking) {
      lx = -sw * 0.8;
      rx = a.state === "idle" ? 1.1 : sw * 0.8;
    } else if (a.state === "working" && atDesk) {
      lx = 1.1 + Math.sin(k * 13) * 0.1;
      rx = 1.1 + Math.cos(k * 11) * 0.1;
      hx = 0.12;
    } else if (a.state === "failed" && atDesk) {
      lx = 2.6;
      rx = 2.6;
      lz = -0.3;
      rz = 0.3;
      hx = 0.35;
      a.body.position.x = still ? 0 : Math.sin(k * 30) * 0.015;
    } else if (a.state === "waiting" && a.loc.kind === "wait") {
      rz = 2.9 + Math.sin(k * 6) * 0.25;
      rx = 0;
      hx = -0.1;
    } else if (a.state === "disabled" && a.loc.kind === "off") {
      lx = 0.3;
      rx = 0.3;
      hx = 0.55 + Math.sin(k * 1.2) * 0.05;
    } else if (a.state === "idle" && a.loc.kind === "lounge") {
      const kind = a.loc.spot.kind, sip = Math.sin(k * 0.9) > 0.8 ? 1 : 0;
      rx = lerp(1.1, 2.3, sip);
      hx = sip * 0.15;
      if (kind === "chat") {
        lx = 0.7 + Math.sin(k * 3.2) * 0.5;
        lz = -0.2;
        hx += Math.sin(k * 2.1) * 0.15;
        setGlyph(a, Math.sin(k * 0.7) > 0 ? "💬" : "☕");
      } else if (kind === "coffee") {
        lx = 1.0;
        setGlyph(a, "☕");
      } else {
        lx = 0.4;
        hx += 0.05;
        setGlyph(a, sip ? "☕" : "😌");
      }
    }
    a.lArm.rotation.x = lx;
    a.rArm.rotation.x = rx;
    a.lArm.rotation.z = lz;
    a.rArm.rotation.z = rz;
    a.head.rotation.x = hx;
    a.bubble.position.y = 2.6 + Math.sin(k * 3) * 0.05;
    if (walking && a.state === "idle") setGlyph(a, "☕");
    if (a.state === "waiting" && walking) setGlyph(a, "✋");

    // The seat's screen: green while working, flashing red when failed.
    const onDesk = atDesk && a.sit > 0.8 && a.loc.desk === a.desk;
    if (onDesk && (a.state === "working" || a.state === "failed")) {
      const m = a.desk.screenMat;
      a.desk.lit = true;
      if (a.state === "working") {
        m.emissive.setHex(COLORS.working);
        m.emissiveIntensity = 0.7 + Math.sin(k * 6) * 0.15;
      } else {
        m.emissive.setHex(COLORS.failed);
        m.emissiveIntensity = still ? 1 : Math.sin(k * 18) > 0 ? 1 : 0.25;
      }
    }
    // A faint data beam from the AI centre while working at the table.
    const on = onDesk && a.state === "working";
    const ln = a.beam, i = a.idx;
    const want = on ? (still ? 0.45 : 0.45 + Math.sin(t * 5 + i) * 0.15) : 0;
    ln.material.opacity = still ? want : lerp(ln.material.opacity, want, Math.min(1, dt * 6));
    ln.visible = ln.material.opacity > 0.01;
    if (ln.visible) {
      const to = scratch;
      a.head.getWorldPosition(to);
      to.y += 0.5;
      const from = handles.aiTop, pos = ln.geometry.attributes.position;
      pos.setXYZ(0, from.x, from.y, from.z);
      pos.setXYZ(1, to.x, to.y, to.z);
      pos.needsUpdate = true;
    }
  }

  function disposeTree(root) {
    root.traverse((o) => {
      if (o.geometry) o.geometry.dispose();
      if (o.material) o.material.dispose();
    });
  }

  return {
    /// Takes `/office/state`'s `loops`: the first MAX_DRAWN get a person and a
    /// seat in list order; the rest only count towards `overflow`.
    sync(loops) {
      const seen = new Set();
      const list = (Array.isArray(loops) ? loops : []).filter((l) => {
        if (!l || typeof l.name !== "string" || seen.has(l.name)) return false;
        seen.add(l.name);
        return true;
      });
      const next = buildLayout(list.length);
      overflow = next.overflow;
      let rebuilt = false, labelScaled = false;
      if (!layout || next.key !== layout.key) {
        if (furniture) {
          scene.remove(furniture);
          disposeTree(furniture);
        }
        layout = next;
        furniture = furnish(scene, THREE, layout);
        freezeStatic(furniture);
        labelScale = Math.min(1, (layout.seats[0].gap * 2) / 2.2);
        rebuilt = true;
        labelScaled = true;
      }
      // With more loops than people drawn, disabled ones (idle on the bench) are the ones left out.
      const drawn = list.length > MAX_DRAWN
        ? [...list.filter((l) => l.state !== "disabled"), ...list.filter((l) => l.state === "disabled")].slice(0, MAX_DRAWN)
        : list;
      const keep = new Set(drawn.map((l) => l.name));
      for (const [name, a] of agents) {
        if (!keep.has(name)) {
          dispose(a);
          agents.delete(name);
        }
      }
      order = drawn.map((l, i) => {
        let a = agents.get(l.name);
        if (!a) {
          a = create(l.name);
          agents.set(l.name, a);
        }
        a.desk = layout.seats[i];
        a.idx = i;
        a.info = l;
        const title = typeof l.title === "string" && l.title ? l.title : l.name;
        const retitled = title !== a.title;
        a.title = title;
        const s = Object.hasOwn(COLORS, l.state) ? l.state : "idle";
        if (s !== a.state) setState(a, s);
        else if (retitled) drawLabel(a);
        return a;
      });
      if (rebuilt) for (const a of order) carryOver(a);
      if (labelScaled) for (const a of agents.values()) scaleLabel(a);
      // New people (and anyone whose spot is gone from a rebuilt layout) appear at their place.
      for (const a of order) if (!a.loc) place(a);
    },
    tick(dt, t) {
      if (!layout) return;
      for (const s of layout.seats) s.lit = false;
      let working = 0;
      for (const a of order) {
        update(a, dt, t);
        if (a.state === "working") working++;
      }
      for (const s of layout.seats) if (!s.lit) s.screenMat.emissive.setHex(0x000000);
      handles.aiRate = 1 + working * 0.5;
    },
    /// The group holding the current layout's tables and chairs (hit-testing hides people behind it).
    furnitureGroup() {
      return furniture;
    },
    /// The agent whose person contains `object3D` (e.g. a raycast hit), or null.
    agentAt(object3D) {
      for (let o = object3D; o; o = o.parent) if (o.userData && o.userData.agent) return agents.get(o.userData.agent) || null;
      return null;
    },
    /// Every drawn person's group, for raycasting.
    pickables() {
      return order.map((a) => a.group);
    },
    /// Highlights the name tags of exactly `names` (hover and selection).
    setHighlight(names) {
      const set = new Set(names);
      for (const a of agents.values()) {
        const hi = set.has(a.name);
        if (hi !== !!a.hi) {
          a.hi = hi;
          scaleLabel(a);
        }
      }
    },
    /// Where `name` stands now as [x, y, z], or null.
    positionOf(name) {
      const a = agents.get(name);
      return a ? [a.group.position.x, a.group.position.y, a.group.position.z] : null;
    },
    /// Test hook: where everyone is.
    debug() {
      return order.map((a) => ({
        name: a.name,
        state: a.state,
        x: Math.round(a.group.position.x * 100) / 100,
        z: Math.round(a.group.position.z * 100) / 100,
        seated: a.sit > 0.5,
        walking: a.path.length > 0,
      }));
    },
    get overflow() {
      return overflow;
    },
  };
}
