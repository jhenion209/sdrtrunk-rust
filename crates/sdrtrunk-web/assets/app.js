// Talkgroups page: a header checkbox `data-select-all="F"` selects or clears every row checkbox of form F (the talkgroup
// batch, the radio name batch).
(function () {
  for (const all of document.querySelectorAll("input[data-select-all]")) {
    all.addEventListener("change", () => {
      for (const box of document.querySelectorAll("input[type=checkbox][form]")) {
        if (box.getAttribute("form") === all.dataset.selectAll) box.checked = all.checked;
      }
    });
  }
})();

// Sidebar: mark the section this page belongs to.
(function () {
  const path = location.pathname;
  for (const a of document.querySelectorAll("nav.side a[data-match]")) {
    const m = a.dataset.match;
    if (m === "/" ? path === "/" : path === m || path.startsWith(m + "/")) a.setAttribute("aria-current", "page");
  }
})();

// Streams and Tuners pages: the table is redrawn by the server every second (`html` events). The server draws it with
// the same template as the page, every value escaped, so the markup is swapped in as is.
(function () {
  for (const el of document.querySelectorAll("[data-live]")) {
    new EventSource(el.dataset.live).addEventListener("html", (e) => { el.innerHTML = e.data; });
  }
})();

// Dashboard: live readouts, summary figures and the call feed (Server-Sent Events from /events).
// Text only ever goes in through textContent, so names from the database cannot inject markup.
(function () {
  const calls = document.getElementById("calls");
  if (!calls) return;
  const set = (id, text) => { const el = document.getElementById(id); if (el) el.textContent = text; };
  const mhz = (hz) => (hz / 1e6).toFixed(4);
  let seen = 0;

  const es = new EventSource("/events");
  // One entry per system; element ids end in "-<system id>".
  es.addEventListener("status", (e) => {
    const all = JSON.parse(e.data);
    for (const s of all) {
      const f = (name, text) => set(`${name}-${s.id}`, text);
      const pill = document.getElementById(`state-${s.id}`);
      if (pill) { pill.textContent = s.state; pill.className = `pill state-${s.state}`; }
      document.getElementById(`panel-${s.id}`)?.setAttribute("data-state", s.state);
      f("error", s.error || "");
      f("tuner", !s.tuner ? "–" : s.tuner.startsWith("rtl:") ? `RTL-SDR ${s.tuner.slice(4)}` : `Airspy ${s.tuner}`);
      f("site", s.site_text);
      f("control", s.control_hz ? `${mhz(s.control_hz)} MHz` : "");
      f("rate", s.tsbk_per_s.toFixed(1));
      f("signal", s.signal);
      f("decode", s.decode);
      f("freq", s.freq);
      f("helpers", s.helpers_text);
      f("correction", s.correction);
      f("dropped", String(s.dropped));
      f("recorded", String(s.recorded));
      f("recording_error", s.recording_error || "");
      f("uptime", s.uptime);
    }
    set("kpi-running", `${all.filter((s) => s.state === "running").length} of ${all.length}`);
    set("kpi-rate", all.reduce((t, s) => t + s.tsbk_per_s, 0).toFixed(1));
    set("kpi-recorded", String(all.reduce((t, s) => t + s.recorded, 0)));
  });
  es.addEventListener("call", (e) => {
    const c = JSON.parse(e.data);
    document.getElementById("nocalls")?.remove();
    const li = document.createElement("li");
    li.classList.toggle("emergency", c.emergency);
    li.classList.toggle("encrypted", c.encrypted);
    const add = (tag, text, cls) => { const el = document.createElement(tag); el.textContent = text; if (cls) el.className = cls; li.appendChild(el); return el; };
    add("time", new Date(c.at_ms).toLocaleTimeString());
    add("strong", c.name ? `${c.name}` : `Talkgroup ${c.group}`);
    add("span", [c.name ? `TG ${c.group}` : "", c.system, `${mhz(c.hz)} MHz`, c.source ? `radio ${c.source}${c.radio_name ? ` (${c.radio_name})` : ""}` : ""].filter(Boolean).join(", "), "muted");
    for (const [on, label] of [[c.emergency, "Emergency"], [c.encrypted, "Encrypted"]]) if (on) add("span", label, "tag" + (label === "Emergency" ? " bad" : ""));
    calls.prepend(li);
    while (calls.children.length > 60) calls.lastElementChild.remove();
    set("kpi-calls", String(++seen));
  });
})();

// Spectrum display: FFT trace and waterfall from the canvas's data-src events (frames of dB, lowest frequency first).
// Used by the tuner spectrum page and by a system's channel panel (which also has FFT/Waterfall view buttons and
// Center/Peak/Display range readouts).
(function () {
  const plot = document.getElementById("spectrum");
  if (!plot) return;
  const fall = document.getElementById("waterfall");
  const pc = plot.getContext("2d"), fc = fall.getContext("2d");
  const readout = document.getElementById("readout"), note = document.getElementById("spectrum-status");
  const holdBtn = document.getElementById("hold"), auto = document.getElementById("auto-range");
  const floorIn = document.getElementById("floor"), ceilIn = document.getElementById("ceiling");
  let hold = false, frame = null, lo = -100, hi = -20;
  holdBtn.addEventListener("click", () => { hold = !hold; holdBtn.textContent = hold ? "Resume" : "Hold"; });
  auto.addEventListener("change", () => { floorIn.disabled = ceilIn.disabled = auto.checked; });
  floorIn.disabled = ceilIn.disabled = auto.checked;

  // Waterfall colours: quiet = dark blue, then cyan, yellow, red for the strongest.
  const stops = [[0, 0, 0, 40], [0.35, 0, 80, 200], [0.6, 0, 210, 220], [0.8, 250, 230, 0], [1, 255, 40, 0]];
  function colour(t) {
    t = Math.min(1, Math.max(0, t));
    for (let i = 1; i < stops.length; i++) {
      const [p1, r1, g1, b1] = stops[i - 1], [p2, r2, g2, b2] = stops[i];
      if (t <= p2) { const k = (t - p1) / (p2 - p1); return [r1 + k * (r2 - r1), g1 + k * (g2 - g1), b1 + k * (b2 - b1)]; }
    }
    return stops[stops.length - 1].slice(1);
  }

  // Automatic range: floor 10 dB under the noise (20th percentile), ceiling above the strongest signal; eased so it does not jump.
  function range(db) {
    if (!auto.checked) {
      const f = Number(floorIn.value), c = Number(ceilIn.value);
      return c > f ? [f, c] : [f, f + 10];
    }
    const sorted = [...db].sort((a, b) => a - b);
    const tLo = sorted[Math.floor(sorted.length * 0.2)] - 10;
    const tHi = Math.max(sorted[sorted.length - 1] + 5, tLo + 30);
    lo += (tLo - lo) * 0.2; hi += (tHi - hi) * 0.2;
    floorIn.value = Math.round(lo); ceilIn.value = Math.round(hi);
    return [lo, hi];
  }

  // FFT or Waterfall, when the page offers the choice (the waterfall keeps filling while hidden).
  const views = [["view-fft", plot], ["view-waterfall", fall]].map(([id, canvas]) => [document.getElementById(id), canvas]);
  for (const [btn] of views) btn?.addEventListener("click", () => {
    for (const [b, canvas] of views) { const on = b === btn; b.setAttribute("aria-pressed", on); canvas.hidden = !on; }
  });
  const show = (id, text) => { const el = document.getElementById(id); if (el) el.textContent = text; };

  // Narrow spans (a single channel) get finer labels.
  let digits = 4;
  const mhz = (hz) => (hz / 1e6).toFixed(digits);

  function draw(f) {
    digits = f.rate_hz < 1e6 ? 5 : 4;
    const W = plot.width, H = plot.height, n = f.db.length, start = f.center_hz - f.rate_hz / 2;
    const [a, b] = range(f.db);
    const y = (d) => H - (d - a) / (b - a) * H;
    const x = (hz) => (hz - start) / f.rate_hz * W;
    pc.clearRect(0, 0, W, H);
    pc.font = "12px system-ui, sans-serif";
    // dB grid every 10 dB, frequency ticks every 0.5 or 1 MHz.
    pc.strokeStyle = "#26303b"; pc.fillStyle = "#8b98a8"; pc.lineWidth = 1;
    for (let d = Math.ceil(a / 10) * 10; d < b; d += 10) {
      pc.beginPath(); pc.moveTo(0, y(d)); pc.lineTo(W, y(d)); pc.stroke();
      pc.fillText(`${d} dB`, 4, y(d) - 3);
    }
    // Largest round step that still gives at least four ticks.
    const step = [1e6, 5e5, 1e5, 5e4, 1e4, 5e3, 1e3].find((s) => f.rate_hz / s >= 4) || 1e3;
    for (let hz = Math.ceil(start / step) * step; hz < start + f.rate_hz; hz += step) {
      pc.beginPath(); pc.moveTo(x(hz), 0); pc.lineTo(x(hz), H); pc.stroke();
      pc.fillText(mhz(hz), x(hz) + 3, H - 4);
    }
    // Band edges outside the usable width are shaded: channels are never placed there.
    if (f.usable_hz) {
      const lo = x(f.center_hz - f.usable_hz / 2), hi = x(f.center_hz + f.usable_hz / 2);
      pc.fillStyle = "rgba(128, 140, 155, 0.18)";
      pc.fillRect(0, 0, lo, H); pc.fillRect(hi, 0, W - hi, H);
    }
    // Markers: control channel green, followed voice channels orange, labels staggered so neighbours stay readable.
    f.markers.forEach((m, i) => {
      const mx = x(m.hz);
      pc.strokeStyle = pc.fillStyle = { control: "#4cd06a", encrypted: "#ff5a52" }[m.kind] || "#ff9f30";
      pc.setLineDash([4, 4]); pc.beginPath(); pc.moveTo(mx, 0); pc.lineTo(mx, H); pc.stroke(); pc.setLineDash([]);
      pc.fillText(m.label, Math.min(mx + 3, W - pc.measureText(m.label).width - 2), 14 + (i % 4) * 14);
    });
    pc.strokeStyle = "#e8f1ff"; pc.lineWidth = 1.25; pc.beginPath();
    f.db.forEach((d, i) => { const px = i / (n - 1) * W; i ? pc.lineTo(px, y(d)) : pc.moveTo(px, y(d)); });
    pc.stroke();
    // Waterfall: shift down one row, newest row on top.
    fc.drawImage(fall, 0, 1);
    const row = fc.createImageData(fall.width, 1);
    for (let i = 0; i < fall.width; i++) {
      const [r, g, bl] = colour((f.db[Math.floor(i * n / fall.width)] - a) / (b - a));
      row.data.set([r, g, bl, 255], i * 4);
    }
    fc.putImageData(row, 0, 0);
    const peak = f.db.reduce((best, d, i) => (d > f.db[best] ? i : best), 0);
    show("r-center", `${mhz(f.center_hz)} MHz`);
    show("r-peak", `${f.db[peak]} dB at ${mhz(start + (peak + 0.5) / n * f.rate_hz)} MHz`);
    show("r-range", `${Math.round(a)} to ${Math.round(b)} dB`);
  }

  plot.addEventListener("mousemove", (e) => {
    if (!frame) return;
    const r = plot.getBoundingClientRect(), t = (e.clientX - r.left) / r.width;
    const i = Math.min(frame.db.length - 1, Math.max(0, Math.round(t * (frame.db.length - 1))));
    readout.textContent = `${mhz(frame.center_hz - frame.rate_hz / 2 + t * frame.rate_hz)} MHz · ${frame.db[i]} dB`;
  });
  plot.addEventListener("mouseleave", () => { readout.textContent = ""; });

  const es = new EventSource(plot.dataset.src);
  es.addEventListener("frame", (e) => { frame = JSON.parse(e.data); note.textContent = ""; if (!hold) draw(frame); });
  es.addEventListener("idle", (e) => { note.textContent = e.data; });
})();

// System signal page: demodulated control channel symbols (milliradians) from /systems/<id>/symbols/events, drawn as
// dots scrolling right to left, with the ideal +3, +1, -1, -3 levels (+/-3pi/4, +/-pi/4) as lines.
(function () {
  const plot = document.getElementById("symbols");
  if (!plot) return;
  const c = plot.getContext("2d"), note = document.getElementById("symbols-status");
  const W = plot.width, H = plot.height, KEEP = 2400, LIMIT = 3600;
  const y = (mrad) => H / 2 - mrad / LIMIT * (H / 2);
  let recent = [];
  function draw() {
    c.clearRect(0, 0, W, H);
    c.font = "12px system-ui, sans-serif";
    c.strokeStyle = "#26303b"; c.fillStyle = "#8b98a8"; c.lineWidth = 1;
    for (const [mrad, label] of [[2356, "+3"], [785, "+1"], [-785, "-1"], [-2356, "-3"]]) {
      c.beginPath(); c.moveTo(0, y(mrad)); c.lineTo(W, y(mrad)); c.stroke();
      c.fillText(label, 4, y(mrad) - 3);
    }
    c.fillStyle = "#e8f1ff";
    recent.forEach((s, i) => c.fillRect(W - (recent.length - i) * W / KEEP, y(s) - 1, 2, 2));
  }
  const es = new EventSource(plot.dataset.src);
  es.addEventListener("symbols", (e) => {
    recent = recent.concat(JSON.parse(e.data).soft).slice(-KEEP);
    note.textContent = "";
    draw();
  });
  es.addEventListener("idle", (e) => { note.textContent = e.data; });
})();
