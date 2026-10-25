// Canvas-drawn art for the office: the wall paintings, the AI graffiti on the
// outer walls, the LoopX logo and the sign plates. Every function takes THREE
// rather than importing it, and draws with the given seeded rng so the room looks
// the same on every load.

function canvasOf(w, h) {
  const c = document.createElement("canvas");
  c.width = w;
  c.height = h;
  return c;
}

function textureOf(THREE, canvas) {
  const t = new THREE.CanvasTexture(canvas);
  t.colorSpace = THREE.SRGBColorSpace;
  return t;
}

const PALETTES = [
  ["#ff6b6b", "#ffd24a", "#4bc0c8", "#2f3a8f"],
  ["#f4a6c3", "#7aa2ff", "#5ee0b5", "#ffc46b"],
  ["#e0732a", "#3f9a4a", "#f2e9d8", "#1f2a44"],
];

/// One of seven abstract paintings: 0 sunset hills, 1 shapes, 2 neural net,
/// 3 waves, 4 bubbles, 5 robot face, 6 grid blocks.
export function paintingTexture(THREE, kind, rng) {
  const w = 512, h = 384, c = canvasOf(w, h), x = c.getContext("2d");
  const pal = PALETTES[Math.floor(rng() * 3)];
  if (kind === 0) {
    const g = x.createLinearGradient(0, 0, 0, h);
    g.addColorStop(0, "#2b3a8f");
    g.addColorStop(0.55, "#ff8a5c");
    g.addColorStop(1, "#ffd98a");
    x.fillStyle = g;
    x.fillRect(0, 0, w, h);
    x.fillStyle = "#fff2b0";
    x.beginPath();
    x.arc(w * 0.65, h * 0.52, 60, 0, 7);
    x.fill();
    [["#3b2a5a", 0.62], ["#2a1f45", 0.75], ["#1a1432", 0.88]].forEach(([col, y], i) => {
      x.fillStyle = col;
      x.beginPath();
      x.moveTo(0, h);
      for (let px = 0; px <= w; px += 16) x.lineTo(px, h * y + Math.sin(px * 0.02 + i * 2) * 22);
      x.lineTo(w, h);
      x.fill();
    });
  } else if (kind === 1) {
    x.fillStyle = "#f2e9d8";
    x.fillRect(0, 0, w, h);
    for (let i = 0; i < 7; i++) {
      x.fillStyle = pal[i % 4];
      const a = rng() * w * 0.7, b = rng() * h * 0.6, s = 60 + rng() * 140;
      if (i % 3 === 0) {
        x.beginPath();
        x.arc(a + s / 2, b + s / 2, s / 2, 0, 7);
        x.fill();
      } else if (i % 3 === 1) {
        x.fillRect(a, b, s, s * 0.6);
      } else {
        x.beginPath();
        x.moveTo(a, b + s);
        x.lineTo(a + s, b + s);
        x.lineTo(a + s / 2, b);
        x.fill();
      }
    }
    x.strokeStyle = "#1f2a44";
    x.lineWidth = 6;
    x.beginPath();
    x.moveTo(0, h * 0.7);
    x.lineTo(w, h * 0.3);
    x.stroke();
  } else if (kind === 2) {
    x.fillStyle = "#0f1226";
    x.fillRect(0, 0, w, h);
    const layers = [3, 5, 5, 2];
    const pts = layers.map((n, li) => Array.from({ length: n }, (_, i) => [60 + li * (w - 120) / 3, h / 2 + (i - (n - 1) / 2) * 62]));
    x.lineWidth = 1.5;
    x.strokeStyle = "rgba(122,162,255,.4)";
    for (let l = 0; l < 3; l++) {
      for (const a of pts[l]) {
        for (const b of pts[l + 1]) {
          x.beginPath();
          x.moveTo(a[0], a[1]);
          x.lineTo(b[0], b[1]);
          x.stroke();
        }
      }
    }
    pts.flat().forEach((p, i) => {
      x.fillStyle = ["#7AA2FF", "#5EE0B5", "#FF8DB8", "#FFC46B"][i % 4];
      x.beginPath();
      x.arc(p[0], p[1], 13, 0, 7);
      x.fill();
    });
  } else if (kind === 3) {
    x.fillStyle = "#fbf3e4";
    x.fillRect(0, 0, w, h);
    for (let i = 0; i < 14; i++) {
      x.strokeStyle = ["#2f3a8f", "#4bc0c8", "#ff6b6b", "#ffd24a"][i % 4];
      x.lineWidth = 5;
      x.beginPath();
      for (let px = 0; px <= w; px += 8) {
        const y = 40 + i * 22 + Math.sin(px * 0.015 + i * 0.5) * 26;
        if (px) x.lineTo(px, y);
        else x.moveTo(px, y);
      }
      x.stroke();
    }
  } else if (kind === 4) {
    x.fillStyle = pal[3];
    x.fillRect(0, 0, w, h);
    x.globalAlpha = 0.55;
    for (let i = 0; i < 22; i++) {
      x.fillStyle = pal[i % 3];
      x.beginPath();
      x.arc(rng() * w, rng() * h, 30 + rng() * 80, 0, 7);
      x.fill();
    }
    x.globalAlpha = 1;
  } else if (kind === 5) {
    x.fillStyle = pal[1];
    x.fillRect(0, 0, w, h);
    x.fillStyle = "#e8ecf2";
    x.beginPath();
    x.roundRect(w * 0.28, h * 0.22, w * 0.44, h * 0.55, 40);
    x.fill();
    x.fillStyle = "#1f2a44";
    [0.4, 0.6].forEach((fx) => {
      x.beginPath();
      x.arc(w * fx, h * 0.45, 28, 0, 7);
      x.fill();
    });
    x.fillStyle = "#5ee0b5";
    [0.4, 0.6].forEach((fx) => {
      x.beginPath();
      x.arc(w * fx, h * 0.45, 12, 0, 7);
      x.fill();
    });
    x.fillRect(w * 0.4, h * 0.62, w * 0.2, 10);
    x.strokeStyle = "#1f2a44";
    x.lineWidth = 8;
    x.beginPath();
    x.moveTo(w / 2, h * 0.22);
    x.lineTo(w / 2, h * 0.1);
    x.stroke();
    x.fillStyle = "#ff6b6b";
    x.beginPath();
    x.arc(w / 2, h * 0.09, 14, 0, 7);
    x.fill();
  } else {
    x.fillStyle = "#f6f2e8";
    x.fillRect(0, 0, w, h);
    x.fillStyle = "#d1453b";
    x.fillRect(20, 20, w * 0.45, h * 0.5);
    x.fillStyle = "#2f5bd1";
    x.fillRect(w * 0.7, h * 0.55, w * 0.25, h * 0.35);
    x.fillStyle = "#ffd24a";
    x.fillRect(w * 0.55, 20, w * 0.15, h * 0.3);
    x.fillStyle = "#111";
    [[w * 0.5, 0, 10, h], [0, h * 0.6, w, 10], [w * 0.7, h * 0.35, w * 0.3, 10], [w * 0.7, h * 0.55, 10, h * 0.45]]
      .forEach((r) => x.fillRect(r[0], r[1], r[2], r[3]));
  }
  return textureOf(THREE, c);
}

const GRAFFITI_PALETTES = [["#ff4d8d", "#ffd24a"], ["#4bc0ff", "#7a5cff"], ["#5ee0b5", "#e8ff6a"], ["#ff8a3d", "#ff4d4d"]];

/// A 2300x700 graffiti mural: `words` are [text, x, y, size, rotation, palette],
/// `extras` are ["robot"|"chip", x, y, size, colour], ["net", x, y, size] or
/// ["spark", x, y, size, colour].
export function graffitiTexture(THREE, words, extras, rng) {
  const w = 2300, h = 700, c = canvasOf(w, h), x = c.getContext("2d");
  x.lineJoin = "round";
  x.lineCap = "round";
  const pals = GRAFFITI_PALETTES;
  function bubble(t, px, py, size, rot, pi) {
    x.save();
    x.translate(px, py);
    x.rotate(rot);
    x.font = "900 " + size + "px Impact,\"Arial Black\",sans-serif";
    const [a, b] = pals[pi % 4];
    x.lineWidth = size * 0.14;
    x.strokeStyle = "#14112B";
    x.strokeText(t, 0, 0);
    const g = x.createLinearGradient(0, -size, 0, 0);
    g.addColorStop(0, a);
    g.addColorStop(1, b);
    x.fillStyle = g;
    x.fillText(t, 0, 0);
    x.lineWidth = size * 0.03;
    x.strokeStyle = "rgba(255,255,255,.85)";
    x.strokeText(t, -size * 0.02, -size * 0.03);
    const tw = x.measureText(t).width;
    for (let i = 0; i < 4; i++) {
      const dx = rng() * tw, len = 20 + rng() * size * 0.35;
      x.strokeStyle = b;
      x.lineWidth = size * 0.04;
      x.beginPath();
      x.moveTo(dx, size * 0.05);
      x.lineTo(dx, size * 0.05 + len);
      x.stroke();
      x.fillStyle = b;
      x.beginPath();
      x.arc(dx, size * 0.05 + len, size * 0.03, 0, 7);
      x.fill();
    }
    x.restore();
  }
  function robot(px, py, s, col) {
    x.save();
    x.translate(px, py);
    x.lineWidth = s * 0.07;
    x.strokeStyle = "#14112B";
    x.fillStyle = col;
    x.beginPath();
    x.roundRect(-s / 2, -s / 2, s, s * 0.8, s * 0.18);
    x.fill();
    x.stroke();
    [-1, 1].forEach((k) => {
      x.fillStyle = "#fff";
      x.beginPath();
      x.arc(k * s * 0.2, -s * 0.1, s * 0.11, 0, 7);
      x.fill();
      x.stroke();
      x.fillStyle = "#14112B";
      x.beginPath();
      x.arc(k * s * 0.2, -s * 0.1, s * 0.04, 0, 7);
      x.fill();
    });
    x.beginPath();
    x.moveTo(-s * 0.2, s * 0.15);
    x.lineTo(s * 0.2, s * 0.15);
    x.stroke();
    x.beginPath();
    x.moveTo(0, -s / 2);
    x.lineTo(0, -s * 0.75);
    x.stroke();
    x.fillStyle = "#ff4d4d";
    x.beginPath();
    x.arc(0, -s * 0.78, s * 0.07, 0, 7);
    x.fill();
    x.stroke();
    x.restore();
  }
  function chip(px, py, s, col) {
    x.save();
    x.translate(px, py);
    x.lineWidth = s * 0.06;
    x.strokeStyle = "#14112B";
    x.fillStyle = col;
    for (let i = -2; i <= 2; i++) {
      const o = i * s * 0.18;
      [[o, -s * 0.62, o, -s * 0.5], [o, s * 0.62, o, s * 0.5], [-s * 0.62, o, -s * 0.5, o], [s * 0.62, o, s * 0.5, o]].forEach((p) => {
        x.beginPath();
        x.moveTo(p[0], p[1]);
        x.lineTo(p[2], p[3]);
        x.stroke();
      });
    }
    x.beginPath();
    x.roundRect(-s / 2, -s / 2, s, s, s * 0.1);
    x.fill();
    x.stroke();
    x.fillStyle = "#14112B";
    x.font = "900 " + s * 0.5 + "px Impact,sans-serif";
    x.textAlign = "center";
    x.fillText("AI", 0, s * 0.17);
    x.restore();
  }
  function net(px, py, s) {
    const L = [3, 4, 3], P = L.map((n, li) => Array.from({ length: n }, (_, i) => [px + li * s * 0.5, py + (i - (n - 1) / 2) * s * 0.3]));
    x.lineWidth = 5;
    x.strokeStyle = "#14112B";
    for (let l = 0; l < 2; l++) {
      for (const a of P[l]) {
        for (const b of P[l + 1]) {
          x.beginPath();
          x.moveTo(a[0], a[1]);
          x.lineTo(b[0], b[1]);
          x.stroke();
        }
      }
    }
    P.flat().forEach((p, i) => {
      x.fillStyle = pals[i % 4][0];
      x.beginPath();
      x.arc(p[0], p[1], s * 0.07, 0, 7);
      x.fill();
      x.stroke();
    });
  }
  function sparkle(px, py, s, col) {
    x.fillStyle = col;
    x.strokeStyle = "#14112B";
    x.lineWidth = 4;
    x.beginPath();
    for (let i = 0; i < 8; i++) {
      const a = i * Math.PI / 4, r = i % 2 ? s * 0.25 : s;
      x.lineTo(px + Math.cos(a) * r, py + Math.sin(a) * r);
    }
    x.closePath();
    x.fill();
    x.stroke();
  }
  words.forEach((a) => bubble(a[0], a[1], a[2], a[3], a[4], a[5]));
  extras.forEach((e) => {
    if (e[0] === "robot") robot(e[1], e[2], e[3], e[4]);
    else if (e[0] === "chip") chip(e[1], e[2], e[3], e[4]);
    else if (e[0] === "net") net(e[1], e[2], e[3]);
    else sparkle(e[1], e[2], e[3], e[4]);
  });
  for (let i = 0; i < 260; i++) {
    x.globalAlpha = 0.35 + rng() * 0.4;
    x.fillStyle = pals[Math.floor(rng() * 4)][Math.floor(rng() * 2)];
    x.beginPath();
    x.arc(rng() * w, rng() * h, 1 + rng() * 4, 0, 7);
    x.fill();
  }
  x.globalAlpha = 1;
  const t = textureOf(THREE, c);
  t.anisotropy = 4;
  return t;
}

/// The LoopX mark on a transparent background: the four-arrow ring, the X inside
/// it and the "LoopX" word mark. Drawn on a 1400x480 canvas (aspect 35:12).
export function logoTexture(THREE) {
  const c = canvasOf(1400, 480), x = c.getContext("2d");
  const cx = 250, cy = 240, r = 140;
  const arcs = [["#7AA2FF", "#2F5BFF", -90], ["#5EE0B5", "#0FA37F", 0], ["#FF8DB8", "#E8336F", 90], ["#FFC46B", "#FF8A00", 180]];
  x.lineCap = "round";
  x.lineWidth = 44;
  for (const [c0, c1, deg] of arcs) {
    const g = x.createLinearGradient(cx - r, cy - r, cx + r, cy + r);
    g.addColorStop(0, c0);
    g.addColorStop(1, c1);
    const a = deg * Math.PI / 180;
    x.strokeStyle = g;
    x.beginPath();
    x.arc(cx, cy, r, a - 0.6, a + 0.6);
    x.stroke();
    x.fillStyle = c1;
    x.beginPath();
    x.arc(cx + Math.cos(a + 0.6) * r, cy + Math.sin(a + 0.6) * r, 28, 0, 7);
    x.fill();
  }
  x.strokeStyle = "#1b1738";
  x.lineWidth = 40;
  x.beginPath();
  x.moveTo(cx - 80, cy - 80);
  x.lineTo(cx + 80, cy + 80);
  x.moveTo(cx - 80, cy + 80);
  x.lineTo(cx + 80, cy - 80);
  x.stroke();
  x.fillStyle = "#14112B";
  x.font = "800 200px -apple-system,system-ui,sans-serif";
  x.fillText("Loop", 470, 290);
  const lw = x.measureText("Loop").width;
  const gx = x.createLinearGradient(470 + lw, 100, 470 + lw + 190, 300);
  gx.addColorStop(0, "#7AA2FF");
  gx.addColorStop(0.5, "#FF8DB8");
  gx.addColorStop(1, "#FF8A00");
  x.fillStyle = gx;
  x.fillText("X", 470 + lw, 290);
  return textureOf(THREE, c);
}

/// A rounded sign plate with one line of (already translated) text, shrunk to fit.
export function signTexture(THREE, text, { width, height, background, color, size }) {
  const c = canvasOf(width, height), x = c.getContext("2d");
  x.fillStyle = background;
  x.beginPath();
  x.roundRect(4, 4, width - 8, height - 8, height * 0.22);
  x.fill();
  let px = size;
  const font = (n) => "600 " + n + "px -apple-system,system-ui,sans-serif";
  x.font = font(px);
  while (px > 12 && x.measureText(text).width > width - 48) {
    px -= 2;
    x.font = font(px);
  }
  x.fillStyle = color;
  x.textAlign = "center";
  x.textBaseline = "middle";
  x.fillText(text, width / 2, height / 2 + 2);
  return textureOf(THREE, c);
}

/// Wall paintings: [kind, x, y, z, width, height, rotationY]. The left (x=-14.96)
/// and right (x=14.96) walls face inward; the rest hang on the back wall between
/// the windows.
const LEFT = [[2, -7.5, 4, 2.4, 1.7], [5, -4, 3.9, 1.4, 1.8], [0, -0.5, 4, 2.4, 1.7], [1, 8, 4.2, 2, 1.4], [4, 10.8, 4, 1.4, 1.5]];
const RIGHT = [[3, -7.5, 4, 2, 1.5], [5, -4, 4, 1.5, 1.9], [2, 4, 4.3, 3, 1.8], [6, 0, 3.9, 1.6, 1.3], [0, 8.5, 4, 2.4, 1.7], [1, 11, 3.9, 1.5, 1.5]];
export function paintingPlacements(z0) {
  return [
    ...LEFT.map(([k, z, y, w, h]) => [k, -14.96, y, z, w, h, Math.PI / 2]),
    ...RIGHT.map(([k, z, y, w, h]) => [k, 14.96, y, z, w, h, -Math.PI / 2]),
    ...[[3, -12], [4, -8], [6, -4]].map(([k, x]) => [k, x, 1.3, z0 + 0.04, 1.5, 1.0, 0]),
    [5, -14.2, 4, z0 + 0.04, 1, 1.6, 0],
    [2, -10, 4, z0 + 0.04, 0.8, 1.4, 0],
  ];
}

/// The two murals on the outer faces of the side walls: x, rotationY, words, extras.
export const GRAFFITI = [
  {
    x: -15.32,
    rotY: -Math.PI / 2,
    words: [["AI", 80, 470, 400, -0.04, 0], ["LLM", 700, 300, 170, 0.05, 1], ["AGENT", 620, 610, 200, -0.03, 2], ["LOOP", 1180, 330, 230, 0.04, 3],
      ["0101", 1230, 610, 140, -0.05, 1], ["THINK", 1560, 500, 230, -0.03, 0], ["BOT", 1960, 330, 170, 0.06, 2]],
    extras: [["robot", 2050, 540, 170, "#7aa2ff"], ["chip", 900, 120, 110, "#ffd24a"], ["net", 1450, 150, 170], ["spark", 560, 120, 44, "#ffd24a"],
      ["spark", 1500, 640, 36, "#ff8dba"]],
  },
  {
    x: 15.32,
    rotY: Math.PI / 2,
    words: [["NEURAL", 90, 440, 250, -0.03, 1], ["AI", 930, 560, 400, 0.03, 3], ["PROMPT", 1250, 330, 190, -0.04, 2], ["LOOP", 1380, 640, 190, 0.04, 0],
      ["0101", 1850, 330, 150, 0.05, 1], ["TRAIN", 1650, 600, 170, -0.03, 3]],
    extras: [["robot", 640, 560, 180, "#5ee0b5"], ["chip", 1980, 520, 120, "#ff8dba"], ["net", 420, 110, 170], ["spark", 900, 120, 44, "#7aa2ff"],
      ["spark", 2200, 120, 40, "#ffd24a"]],
  },
];
