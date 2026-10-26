// The office's three colleagues, seated around the round desk. Every loop belongs to one
// of them (sorted by name, dealt out in turn): while one of their loops runs they type at
// their screen, while one waits for approval they raise a hand, while one failed they hold
// their head. A question asked in the chat sends a randomly picked colleague to work until
// the answer is done. People with nothing to do now and then turn to each other and talk
// about something worth discussing (a speech bubble). During the workday (09:00-17:00 on the
// scene's clock) an idle colleague now and then walks out to the plaza in front of the office,
// to the coffee shop (mostly in the morning) or the bookstore (mostly in the evening), stays a
// while and comes back; at most one is away at a time and anyone who gets work returns at once.
// Name tags, glyph bubbles and speech are canvas sprites, so a loop name is always text, never
// markup. Working people get a faint data beam to the AI centre rack.

const V = new URL(import.meta.url).search;
const [{ rng, freezeStatic }, { buildLayout, furnish, SEATS, DESK }, { appearanceFor, buildPerson }, { SHOPS, PLAZA_Z, DOOR_APPROACH_Z }] = await Promise.all([
  import("./world.js" + V),
  import("./layout.js" + V),
  import("./people.js" + V),
  import("./shops.js" + V),
]);

/// The colleagues, in seat order. Names are fixed.
export const NAMES = ["E1", "E2", "E3"];

const COLORS = { working: 0x3ddc84, waiting: 0xffb020, idle: 0x7d8590, failed: 0xff5252 };
const GLYPHS = { working: "⌨️", waiting: "✋", failed: "⚠️" };
const TRIP_GLYPH = { coffee: "☕", books: "📖" };
/// A requested run (Run now) is shown as work this long while the loop's own state catches up.
const PENDING_RUN_S = 25;
/// A colleague who finished a chat answer keeps typing this long, so a quick one is still seen.
const MIN_DESK_S = 3.5;
/// A chat takes this long to start talking, how long one exchange lasts and how long each speaks.
const FIRST_TALK_S = 4, TALK_S = 9, TURN_S = 3;
/// How fast arms, legs and head ease into the next pose (per second).
const POSE_EASE = 9;
/// Walking pace, the workday on the scene's clock, how long a visit lasts and the gap between trips.
const WALK_SPEED = 2.6;
const WORK_START = 9, WORK_END = 17;
const STAY_S = [12, 20], TRIP_GAP_S = [25, 50], FIRST_TRIP_S = 12;
/// Radius of the walk around the desk, clear of the chairs, and the exit point on the office's front edge.
const RING = DESK.seat + 1.2, EXIT_Z = 13.2;
/// Lines of an assigned conversation, in order; `{topic}` is what the user picked or typed.
const TALK_LINES = ["talk_line_1", "talk_line_2", "talk_line_3", "talk_line_4"];
const HUMAN_TOPICS = ["human_1", "human_2", "human_3", "human_4", "human_5", "human_6", "human_7", "human_8"];
const GENERIC_TOPICS = ["topic_1", "topic_2", "topic_3", "topic_4", "topic_5"];

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
/// (`#office-i18n`), reducedMotion}. Under reduced motion nobody turns or talks, and
/// poses hold still.
export function createAgents(ctx) {
  const { THREE, scene, handles } = ctx;
  const strings = ctx.strings || {};
  const still = !!ctx.reducedMotion;
  const R = rng(5);
  const scratch = new THREE.Vector3();
  const layout = buildLayout();
  const furniture = furnish(scene, THREE, layout);
  freezeStatic(furniture);
  const people = [];
  let clock = 0, loops = [], chatter = null, talk = null, nextTalk = FIRST_TALK_S, hour = 13, nextTrip = FIRST_TRIP_S;

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
    a.label.scale.set(2.8 * k, 0.875 * k, 1);
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
    x.font = "600 54px -apple-system,system-ui,sans-serif";
    x.fillText(a.title, c.width / 2, 70);
    const max = c.width - 40;
    const text = (a.trip && a.state === "idle" ? strings["at_" + a.trip.place] : strings["state_" + a.state]) || "";
    x.font = "500 40px -apple-system,system-ui,sans-serif";
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
  /// Wraps `text` into the speech bubble (or hides the bubble when it is empty).
  function setSpeech(a, text) {
    if (a.said === text) return;
    a.said = text;
    a.speech.visible = !!text;
    const { c, tex } = a.speech.userData, x = c.getContext("2d");
    x.clearRect(0, 0, c.width, c.height);
    if (!text) return;
    x.font = "500 38px -apple-system,system-ui,sans-serif";
    const lines = [], max = c.width - 70;
    let line = "";
    for (const word of text.split(/(?<=\s)|(?<=[　-鿿，。？！])/u)) {
      if (line && x.measureText(line + word).width > max) {
        lines.push(line.trimEnd());
        line = "";
      }
      line += word;
    }
    if (line) lines.push(line.trimEnd());
    const shown = lines.slice(0, 3), h = 40 + shown.length * 48;
    const top = c.height - 28 - h;
    x.fillStyle = "rgba(255,255,255,.96)";
    x.beginPath();
    x.roundRect(8, top, c.width - 16, h, 30);
    x.fill();
    x.beginPath();
    x.moveTo(c.width / 2 - 16, top + h - 2);
    x.lineTo(c.width / 2, c.height - 6);
    x.lineTo(c.width / 2 + 16, top + h - 2);
    x.fill();
    x.fillStyle = "#1b1d22";
    x.textAlign = "center";
    x.textBaseline = "middle";
    shown.forEach((l, i) => x.fillText(l, c.width / 2, top + 20 + 24 + i * 48));
    tex.needsUpdate = true;
  }

  // ---------- people ----------
  function create(name, i) {
    const p = buildPerson(THREE, appearanceFor(name));
    p.group.userData.agent = name;
    scene.add(p.group);
    const label = sprite(2.8, 0.875, 512, 160);
    label.position.set(0, 3.3, 0);
    label.material.opacity = 0.92;
    p.group.add(label);
    const bubble = sprite(0.8, 0.8, 128, 128);
    bubble.position.set(0, 2.6, 0);
    p.group.add(bubble);
    const speech = sprite(3.6, 1.7, 640, 300);
    speech.position.set(0, 4.7, 0);
    speech.visible = false;
    p.group.add(speech);
    const beam = new THREE.Line(
      new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(), new THREE.Vector3()]),
      new THREE.LineBasicMaterial({ color: 0x6aa8ff, transparent: true, opacity: 0 })
    );
    beam.frustumCulled = false;
    beam.visible = false;
    scene.add(beam);
    const seat = layout.seats[i];
    const a = {
      name, title: name, idx: i, seat, state: null, loops: [], ...p, label, bubble, speech, beam,
      said: null, glyph: null, chat: false, pendingUntil: 0, t0: phaseOf(name), sit: 1, wantSit: true, trip: null, path: [],
      yaw: 0, yawT: 0, pose: { lx: 0.15, rx: 0.15, lz: 0, rz: 0, hx: 0, bx: 0 },
    };
    // Seated at once, facing the plant.
    p.group.position.set(seat.x, 0, seat.z);
    a.faceYaw = Math.atan2(-(seat.face[0] - seat.x), -(seat.face[1] - seat.z));
    a.yaw = a.yawT = a.faceYaw;
    p.group.rotation.y = a.yaw;
    return a;
  }
  function setState(a, s) {
    a.state = s;
    drawLabel(a);
    setGlyph(a, a.trip && s === "idle" ? TRIP_GLYPH[a.trip.place] : GLYPHS[s] || null);
    a.mug.visible = s === "idle" && (!a.trip || a.trip.place === "coffee");
  }
  /// What `a` is doing now, from their loops, a chat answer in progress or a requested run.
  function stateOf(a) {
    if (a.chat || a.pendingUntil > clock || a.loops.some((l) => l.state === "working")) return "working";
    if (a.loops.some((l) => l.state === "waiting")) return "waiting";
    if (a.loops.some((l) => l.state === "failed")) return "failed";
    return "idle";
  }
  const ownerOf = (loopName) => people.find((a) => a.loops.some((l) => l.name === loopName)) || null;
  const view = (a) => ({
    name: a.name, title: a.title, state: a.state, answering: a.chat, loops: a.loops,
    free: a.state === "idle" && !a.chat, away: a.trip ? a.trip.place : null,
    note: a.chat ? "answering" : a.trip ? "at_" + a.trip.place : null,
  });

  // ---------- talking ----------
  const titleOf = (l) => (typeof l.title === "string" && l.title ? l.title : l.name);
  /// Something for `a` to bring up: a real loop's state when there is one, else a general topic.
  function topicFor() {
    const pool = GENERIC_TOPICS.map((key) => ({ key }));
    for (const l of loops) {
      const key = l.state === "failed" ? "topic_failed" : l.state === "waiting" ? "topic_waiting" : l.state === "disabled" ? "topic_disabled" : "topic_ok";
      // Real loops come up more often than the general topics.
      for (let i = 0; i < 3; i++) pool.push({ key, loop: titleOf(l) });
    }
    const t = pool[Math.floor(R() * pool.length)];
    return (strings[t.key] || "").replace("{loop}", t.loop || "");
  }
  function startTalk() {
    const idle = people.filter((a) => a.state === "idle" && !a.trip);
    if (idle.length < 2) return false;
    const i = Math.floor(R() * idle.length);
    const a = idle[i], b = idle[(i + 1 + Math.floor(R() * (idle.length - 1))) % idle.length];
    talk = { a, b, t: 0, turn: 0, speaker: R() < 0.5 ? a : b };
    say(talk.speaker, talk.speaker === a ? b : a);
    return true;
  }
  function say(speaker, listener) {
    let text;
    if (talk && talk.topic) text = (strings[TALK_LINES[talk.line++]] || "").replaceAll("{topic}", talk.topic);
    else text = topicFor();
    if (!text) return;
    setSpeech(speaker, text);
    setSpeech(listener, null);
  }
  function endTalk() {
    if (!talk) return;
    setSpeech(talk.a, null);
    setSpeech(talk.b, null);
    talk = null;
    nextTalk = 6 + R() * 8;
  }
  const talking = (a) => !!talk && (talk.a === a || talk.b === a);
  function stepTalk(dt) {
    if (still) return;
    if (talk) {
      // Whoever gets work ends the conversation.
      if (talk.a.state !== "idle" || talk.b.state !== "idle") return endTalk();
      talk.t += dt;
      talk.turn += dt;
      if (talk.t >= (talk.topic ? TURN_S * TALK_LINES.length : TALK_S)) return endTalk();
      if (talk.turn >= TURN_S) {
        talk.turn = 0;
        talk.speaker = talk.speaker === talk.a ? talk.b : talk.a;
        say(talk.speaker, talk.speaker === talk.a ? talk.b : talk.a);
      }
    } else if ((nextTalk -= dt) < 0 && !startTalk()) {
      nextTalk = 3 + R() * 4;
    }
  }

  // ---------- trips to the shops ----------
  const inWorkday = () => hour >= WORK_START && hour < WORK_END;
  const between = ([lo, hi]) => lo + R() * (hi - lo);
  /// The points from `a`'s seat to a visitor's spot in the shop: slide out past the chair, round the
  /// desk at RING to the open front, across the plaza to the door and in.
  function routeOut(a, place) {
    const shop = SHOPS[place], spot = shop.spots[a.idx % shop.spots.length];
    const at = (ang, r) => [DESK.x + Math.cos(ang) * r, DESK.z + Math.sin(ang) * r];
    const target = Math.PI / 2, from = a.seat.angle, d = angDiff(from, target), dir = d >= 0 ? 1 : -1;
    const slide = from + dir * 0.35, steps = Math.max(1, Math.ceil((Math.abs(d) - 0.35) / 0.4));
    const pts = [at(slide, DESK.seat), at(slide, RING)];
    for (let i = 1; i <= steps; i++) pts.push(at(slide + dir * (Math.abs(d) - 0.35) * (i / steps), RING));
    pts.push([DESK.x, EXIT_Z], [shop.cx, PLAZA_Z], [shop.cx, DOOR_APPROACH_Z], [shop.cx, shop.door[1] + 0.3], [shop.cx, shop.door[1] + 1.4], [spot.x, spot.z]);
    return { pts, spot };
  }
  function startTrip(a, place) {
    const { pts, spot } = routeOut(a, place);
    a.trip = { place, phase: "out", spot, done: [], stay: 0 };
    a.path = pts;
    a.wantSit = false;
    drawLabel(a);
    setState(a, a.state);
  }
  /// Heads home along the way it came (also from inside the shop).
  function beginBack(a) {
    const back = [...a.trip.done].reverse();
    // Drop the spot it stands on, if that is the first point.
    const p = a.group.position;
    if (back.length && Math.hypot(back[0][0] - p.x, back[0][1] - p.z) < 0.2) back.shift();
    a.path = [...back, [a.seat.x, a.seat.z]];
    a.trip.phase = "back";
    a.trip.done = [];
  }
  function endTrip(a) {
    a.trip = null;
    nextTrip = between(TRIP_GAP_S);
    drawLabel(a);
    setState(a, a.state);
  }
  /// Someone idle goes out (to `place`, else by the time of day): mostly coffee in the morning,
  /// books in the evening, either at midday. Only one at a time, never someone busy or talking.
  function pickTrip(place) {
    if (people.some((a) => a.trip)) return false;
    const free = people.filter((a) => a.state === "idle" && !talking(a) && !a.chat && a.pendingUntil <= clock);
    if (!free.length) return false;
    const a = free[Math.floor(R() * free.length)];
    const coffeeShare = hour < 12 ? 0.75 : hour < 14 ? 0.5 : 0.25;
    startTrip(a, place || (R() < coffeeShare ? "coffee" : "books"));
    return true;
  }
  function stepTrips(dt) {
    if (still) return;
    if (!people.some((a) => a.trip) && inWorkday() && (nextTrip -= dt) < 0 && !pickTrip()) nextTrip = 4 + R() * 4;
    for (const a of people) {
      const trip = a.trip;
      if (!trip) continue;
      // Work (a loop, the chat) calls them back at once.
      if (trip.phase !== "back" && a.state !== "idle") beginBack(a);
      if (trip.phase === "out" && !a.path.length) {
        trip.phase = "in";
        trip.stay = between(STAY_S);
      } else if (trip.phase === "in") {
        if ((trip.stay -= dt) <= 0) beginBack(a);
      } else if (trip.phase === "back" && !a.path.length) {
        a.wantSit = true;
        if (a.sit > 0.95) endTrip(a);
      }
    }
  }

  // ---------- animation ----------
  function update(a, dt, t) {
    const k = still ? a.t0 : t + a.t0;
    const talkingTo = talking(a) ? (talk.a === a ? talk.b : talk.a) : null;
    // Walking: stand first, then follow the path; facing follows the way, or the shop's counter/shelves.
    let walking = false;
    const p = a.group.position;
    a.sit = still ? (a.wantSit ? 1 : 0) : lerp(a.sit, a.wantSit ? 1 : 0, Math.min(1, dt * 6));
    if (a.path.length && a.sit < 0.15) {
      const [tx, tz] = a.path[0];
      const dx = tx - p.x, dz = tz - p.z, d = Math.hypot(dx, dz), step = WALK_SPEED * dt;
      if (d <= step) {
        p.x = tx;
        p.z = tz;
        a.trip.done.push(a.path.shift());
      } else {
        p.x += (dx / d) * step;
        p.z += (dz / d) * step;
        a.yawT = Math.atan2(-dx, -dz);
        walking = true;
      }
    } else if (a.trip && a.trip.phase === "in") {
      a.yawT = Math.atan2(-(a.trip.spot.face[0] - p.x), -(a.trip.spot.face[1] - p.z));
    } else if (talkingTo) {
      const q = talkingTo.group.position;
      a.yawT = Math.atan2(-(q.x - p.x), -(q.z - p.z));
    } else if (!a.trip || a.trip.phase === "back") {
      if (!a.path.length) a.yawT = a.faceYaw;
    }
    a.yaw = still ? a.yawT : a.yaw + angDiff(a.yaw, a.yawT) * Math.min(1, dt * 8);
    a.group.rotation.y = a.yaw;
    const sit = a.sit, sw = walking ? Math.sin(t * 9) * 0.6 : 0;
    a.body.position.y = lerp(0, -0.37, sit);
    a.legL.hip.rotation.x = lerp(sw, 1.5, sit);
    a.legR.hip.rotation.x = lerp(-sw, 1.5, sit);
    a.legL.knee.rotation.x = lerp(walking ? Math.max(0, -sw) * 0.8 : 0, -1.5, sit);
    a.legR.knee.rotation.x = lerp(walking ? Math.max(0, sw) * 0.8 : 0, -1.5, sit);
    const atDesk = !a.trip && sit > 0.8;

    let lx = 0.15, rx = 0.15, lz = 0, rz = 0, hx = 0, bx = 0, swL = 0, swR = 0;
    const visiting = a.trip && a.trip.phase === "in" && !walking;
    if (walking) {
      // The swing itself is not eased; only the pose it swings around is. A mug stays in the right hand.
      swL = -sw * 0.8;
      if (a.trip && a.trip.place === "coffee") rx = 1.1;
      else swR = sw * 0.8;
    } else if (visiting && a.trip.place === "coffee") {
      const sip = Math.sin(k * 0.9) > 0.7 ? 1 : 0;
      lx = 0.5;
      rx = lerp(1.1, 2.3, sip);
      hx = sip * 0.15;
    } else if (visiting) {
      // Browsing: a book held open in both hands, head bent over it.
      lx = 1.2;
      rx = 1.2;
      hx = 0.4 + Math.sin(k * 0.8) * 0.05;
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
      bx = still ? 0 : Math.sin(k * 30) * 0.015;
    } else if (a.state === "waiting") {
      lx = 0.9;
      rz = 2.9 + Math.sin(k * 6) * 0.25;
      rx = 0;
      hx = -0.1;
    } else if (talkingTo) {
      const speaking = talk.speaker === a;
      lx = 0.9;
      rx = speaking ? 1.0 + Math.sin(k * 4) * 0.5 : 0.9;
      lz = speaking ? -0.3 : 0;
      hx = speaking ? Math.sin(k * 5) * 0.08 : Math.sin(k * 2.4) * 0.12;
    } else {
      const sip = Math.sin(k * 0.9) > 0.8 ? 1 : 0;
      lx = 0.9;
      rx = lerp(1.0, 2.3, sip);
      hx = sip * 0.15;
    }
    // Ease into each pose so changing activity never snaps a limb.
    const ps = a.pose, e = still ? 1 : Math.min(1, dt * POSE_EASE);
    ps.lx = lerp(ps.lx, lx, e);
    ps.rx = lerp(ps.rx, rx, e);
    ps.lz = lerp(ps.lz, lz, e);
    ps.rz = lerp(ps.rz, rz, e);
    ps.hx = lerp(ps.hx, hx, e);
    ps.bx = lerp(ps.bx, bx, e);
    a.lArm.rotation.x = ps.lx + swL;
    a.rArm.rotation.x = ps.rx + swR;
    a.lArm.rotation.z = ps.lz;
    a.rArm.rotation.z = ps.rz;
    a.head.rotation.x = ps.hx;
    a.body.position.x = ps.bx;
    a.bubble.position.y = 2.6 + Math.sin(k * 3) * 0.05;

    // The seat's screen: green while working, flashing red when failed.
    if (atDesk && (a.state === "working" || a.state === "failed")) {
      const m = a.seat.screenMat;
      a.seat.lit = true;
      if (a.state === "working") {
        m.emissive.setHex(COLORS.working);
        m.emissiveIntensity = 0.7 + Math.sin(k * 6) * 0.15;
      } else {
        m.emissive.setHex(COLORS.failed);
        m.emissiveIntensity = still ? 1 : Math.sin(k * 18) > 0 ? 1 : 0.25;
      }
    }
    // A faint data beam from the AI centre while working at the desk.
    const on = atDesk && a.state === "working";
    const ln = a.beam;
    const want = on ? (still ? 0.45 : 0.45 + Math.sin(t * 5 + a.idx) * 0.15) : 0;
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

  function refreshStates() {
    for (const a of people) {
      const s = stateOf(a);
      if (s !== a.state) setState(a, s);
    }
  }
  for (let i = 0; i < SEATS; i++) people.push(create(NAMES[i], i));
  refreshStates();

  return {
    /// Takes `/office/state`'s `loops`: sorted by name and dealt out to the three colleagues in turn.
    sync(next) {
      const seen = new Set();
      loops = (Array.isArray(next) ? next : []).filter((l) => {
        if (!l || typeof l.name !== "string" || seen.has(l.name)) return false;
        seen.add(l.name);
        return true;
      });
      const sorted = [...loops].sort((x, y) => (x.name < y.name ? -1 : x.name > y.name ? 1 : 0));
      for (const a of people) a.loops = [];
      sorted.forEach((l, i) => people[i % SEATS].loops.push(l));
      refreshStates();
    },
    tick(dt, t) {
      clock += dt;
      refreshStates();
      stepTrips(dt);
      stepTalk(dt);
      for (const s of layout.seats) s.lit = false;
      let working = 0;
      for (const a of people) {
        update(a, dt, t);
        if (a.state === "working") working++;
      }
      for (const s of layout.seats) if (!s.lit) s.screenMat.emissive.setHex(0x000000);
      handles.aiRate = 1 + working * 0.5;
    },
    /// The group holding the desk, plant and chairs (hit-testing hides people behind it).
    furnitureGroup() {
      return furniture;
    },
    /// The colleague whose person contains `object3D` (e.g. a raycast hit), or null.
    agentAt(object3D) {
      for (let o = object3D; o; o = o.parent) if (o.userData && o.userData.agent) return people.find((a) => a.name === o.userData.agent) || null;
      return null;
    },
    /// Every colleague's group, for raycasting.
    pickables() {
      return people.map((a) => a.group);
    },
    /// Highlights the name tags of exactly `names` (hover and selection).
    setHighlight(names) {
      const set = new Set(names);
      for (const a of people) {
        const hi = set.has(a.name);
        if (hi !== !!a.hi) {
          a.hi = hi;
          scaleLabel(a);
        }
      }
    },
    /// Where `name` sits as [x, y, z], or null.
    positionOf(name) {
      const a = people.find((p) => p.name === name);
      return a ? [a.group.position.x, a.group.position.y, a.group.position.z] : null;
    },
    /// `{name, title, state, answering, loops}` for one colleague, or null.
    employee(name) {
      const a = people.find((p) => p.name === name);
      return a ? view(a) : null;
    },
    employees() {
      return people.map(view);
    },
    /// A run was just started for the loop `loopName` (Run now, a chat command): its owner
    /// works now instead of waiting for the state feed to show the run.
    runRequested(loopName) {
      const a = ownerOf(loopName);
      if (!a || still) return;
      a.pendingUntil = clock + PENDING_RUN_S;
    },
    /// The chat started (`busy`) or finished answering. `name` (tests) picks who; otherwise a
    /// random colleague, preferring one who is not busy (with a loop or an earlier answer).
    chatBusy(busy, name) {
      if (busy) {
        if (chatter) return;
        // Free first; else anyone not running a loop (still finishing an answer); else anyone.
        const noLoop = people.filter((a) => !a.loops.some((l) => l.state === "working") && !a.trip);
        const free = noLoop.filter((a) => a.pendingUntil <= clock);
        const pool = free.length ? free : noLoop.length ? noLoop : people;
        chatter = people.find((a) => a.name === name) || pool[Math.floor(Math.random() * pool.length)];
        chatter.chat = true;
      } else if (chatter) {
        chatter.chat = false;
        chatter.pendingUntil = Math.max(chatter.pendingUntil, clock + MIN_DESK_S);
        chatter = null;
      }
      refreshStates();
    },
    /// A chat tool call is running (reading mail, an issue...): someone is on it even if the
    /// start event was missed.
    toolRequested() {
      if (!chatter) this.chatBusy(true);
    },
    /// The user sends `name` to `place` ("coffee" or "books"), at any hour. False when they are
    /// busy (a loop, an answer), already out, or the scene is still (reduced motion).
    sendTo(name, place) {
      const a = people.find((p) => p.name === name);
      if (still || !a || !SHOPS[place] || a.trip || a.state !== "idle" || a.chat) return false;
      if (talking(a)) endTalk();
      startTrip(a, place);
      return true;
    },
    /// The user calls `name` back from the shop.
    callBack(name) {
      const a = people.find((p) => p.name === name);
      if (!a || !a.trip || a.trip.phase === "back") return false;
      beginBack(a);
      return true;
    },
    /// The user has `name` talk with `otherName` about `topic` (free text, or a random everyday
    /// topic when empty). False when either is busy or out.
    chatWith(name, otherName, topic) {
      const a = people.find((p) => p.name === name), b = people.find((p) => p.name === otherName);
      const ok = (x) => x && !x.trip && x.state === "idle" && !x.chat;
      if (still || !ok(a) || !ok(b) || a === b) return false;
      endTalk();
      const text = String(topic || "").trim().slice(0, 60) || strings[HUMAN_TOPICS[Math.floor(R() * HUMAN_TOPICS.length)]] || "";
      talk = { a, b, t: 0, turn: 0, speaker: a, topic: text, line: 0 };
      say(a, b);
      return true;
    },
    /// Test hook: who is where and what they are doing.
    debug() {
      return people.map((a) => ({
        name: a.name,
        state: a.state,
        x: Math.round(a.group.position.x * 100) / 100,
        z: Math.round(a.group.position.z * 100) / 100,
        seated: a.sit > 0.5,
        walking: a.path.length > 0,
        at: a.trip ? (a.trip.phase === "in" ? a.trip.place : "walking") : "desk",
        away: a.trip ? a.trip.place : null,
        loops: a.loops.map((l) => l.name),
        chat: a.chat,
        talking: talking(a),
        said: a.said,
      }));
    },
    /// The scene's hour of day (fractional): trips happen only during the workday, 09:00-17:00.
    setHour(h) {
      if (Number.isFinite(h)) hour = h;
    },
    /// Test hook: send the first idle colleague to `place` ("coffee" or "books") now, whatever the hour.
    visitNow(place) {
      if (still || !SHOPS[place]) return false;
      return pickTrip(place);
    },
    /// Test hook: start a conversation between the first two idle colleagues now.
    talkNow() {
      endTalk();
      return startTalk();
    },
    get overflow() {
      return 0;
    },
  };
}
