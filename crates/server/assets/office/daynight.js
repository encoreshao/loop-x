// Day and night follow the clock: sunrise at 06:00, sunset at 18:00, with a
// smoothstep twilight. Moves the sun and moon along their arcs, repaints the sky
// gradient, fades the stars in, and sets the lights and the world's night
// materials (lamps, car lights, lit windows, grass, clouds) through the handles
// buildWorld returned.

export function createDayNight(THREE, scene, handles, lights) {
  const env = { day: 1, tw: 0 };
  const C = THREE.Color;
  const mix = (a, b, t) => new C(a).lerp(new C(b), t);
  const lerp = (a, b, t) => a + (b - a) * t;
  const hex = (c) => "#" + c.getHexString();
  const { sky, sun, moon, stars } = handles;

  /// Hours since midnight (fractional): `override` when it is a number, else the
  /// local clock.
  function currentHour(override) {
    if (typeof override === "number" && Number.isFinite(override)) return override;
    const d = new Date();
    return d.getHours() + d.getMinutes() / 60;
  }

  function apply(hour) {
    const th = ((hour - 6) / 12) * Math.PI, e = Math.sin(th);
    const dayF = THREE.MathUtils.smoothstep(e, -0.22, 0.3), nightF = 1 - dayF;
    const tw = Math.exp(-Math.pow(e / 0.2, 2)) * 0.9;

    const top = mix(0x050816, 0x2f78d2, dayF);
    const mid = mix(0x0b1330, 0x7fb8ef, dayF).lerp(new C(0xc9608a), tw * 0.5);
    const hor = mix(0x1a2347, 0xe4f2fc, dayF).lerp(new C(0xff9a5c), tw * 0.85);
    const g = sky.ctx.createLinearGradient(0, 0, 0, sky.canvas.height);
    g.addColorStop(0, hex(top));
    g.addColorStop(0.35, hex(mid));
    g.addColorStop(0.5, hex(hor));
    g.addColorStop(1, hex(hor));
    sky.ctx.fillStyle = g;
    sky.ctx.fillRect(0, 0, sky.canvas.width, sky.canvas.height);
    sky.tex.needsUpdate = true;
    scene.background = hor;

    sun.mesh.position.set(-Math.cos(th) * 80, Math.sin(th) * 55, -150);
    sun.halo.position.copy(sun.mesh.position);
    sun.halo.position.z += 1;
    const tm = th + Math.PI;
    moon.mesh.position.set(-Math.cos(tm) * 80, Math.sin(tm) * 55, -150);
    moon.halo.position.copy(moon.mesh.position);
    moon.halo.position.z += 1;
    sun.mesh.visible = sun.halo.visible = e > -0.1;
    moon.mesh.visible = moon.halo.visible = Math.sin(tm) > -0.1;
    stars.material.opacity = Math.max(0, nightF - 0.15);

    lights.sun.intensity = lerp(0.25, 1.0, dayF);
    lights.sun.color.copy(mix(0x8fa8ff, 0xfff1dd, dayF)).lerp(new C(0xff9a5c), tw * 0.6);
    lights.hemi.intensity = lerp(0.3, 0.8, dayF);
    lights.hemi.color.copy(mix(0x3a4a7a, 0xdfe8ff, dayF));
    lights.warm.intensity = lerp(90, 40, dayF);
    for (const l of lights.tableLights) l.intensity = nightF * 55 + tw * 12;

    env.day = dayF;
    env.tw = tw;
    handles.setNight(env);
  }

  return { apply, currentHour, env };
}
