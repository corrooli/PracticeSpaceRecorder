// Full-width waveform bay. Steel while listening, amber while recording.
// Data arrives as (peak, rms) byte pairs at 100 bins/s per track; recorded
// takes go into a max-pyramid so a 3 hour take still redraws in well under a ms.

const DB_RANGE = 48;
const LIVE_SECONDS = 30;

class Pyramid {
  levels: Uint8Array[] = [new Uint8Array(4096)];
  lens: number[] = [0];

  clear() {
    this.levels = [new Uint8Array(4096)];
    this.lens = [0];
  }

  get length() {
    return this.lens[0];
  }

  push(pk: number, rms: number) {
    this.add(0, pk, rms);
  }

  private add(l: number, pk: number, rms: number) {
    if (l >= this.levels.length) {
      this.levels.push(new Uint8Array(4096));
      this.lens.push(0);
    }
    let a = this.levels[l];
    const n = this.lens[l];
    if ((n + 1) * 2 > a.length) {
      const b = new Uint8Array(a.length * 2);
      b.set(a);
      this.levels[l] = a = b;
    }
    a[n * 2] = pk;
    a[n * 2 + 1] = rms;
    this.lens[l] = n + 1;
    if ((n & 1) === 1) {
      this.add(l + 1, Math.max(a[n * 2 - 2], pk), Math.max(a[n * 2 - 1], rms));
    }
  }

  /** Max (peak, rms) over bins [a, b). */
  query(a: number, b: number, out: Float32Array) {
    let span = Math.max(1, b - a);
    let l = 0;
    while (span >= 4 && l + 1 < this.levels.length && this.lens[l + 1] > 0) {
      span /= 2;
      l++;
    }
    const s = 1 << l;
    const lv = this.levels[l];
    const n = this.lens[l];
    let i0 = Math.floor(a / s), i1 = Math.ceil(b / s);
    if (i1 <= i0) i1 = i0 + 1;
    if (i1 > n) i1 = n;
    let pk = 0, rms = 0;
    for (let i = Math.max(0, i0); i < i1; i++) {
      if (lv[i * 2] > pk) pk = lv[i * 2];
      if (lv[i * 2 + 1] > rms) rms = lv[i * 2 + 1];
    }
    out[0] = pk;
    out[1] = rms;
  }
}

class Ring {
  buf: Uint8Array;
  head = 0;
  len = 0;
  constructor(public cap: number) {
    this.buf = new Uint8Array(cap * 2);
  }
  push(pk: number, rms: number) {
    this.buf[this.head * 2] = pk;
    this.buf[this.head * 2 + 1] = rms;
    this.head = (this.head + 1) % this.cap;
    if (this.len < this.cap) this.len++;
  }
  clear() {
    this.head = 0;
    this.len = 0;
  }
  /** i = 0 is the oldest of the last `cap` bins (zero-padded when not full). */
  at(i: number, out: Float32Array) {
    const pad = this.cap - this.len;
    if (i < pad) { out[0] = 0; out[1] = 0; return; }
    const k = (this.head - this.len + (i - pad) + this.cap * 2) % this.cap;
    out[0] = this.buf[k * 2];
    out[1] = this.buf[k * 2 + 1];
  }
}

type Palette = { edge: string; peakTop: string; peakMid: string; rmsTop: string; rmsMid: string; glow: string };
const STEEL: Palette = { edge: "rgba(190,208,230,0.85)", peakTop: "rgba(120,142,170,0.30)", peakMid: "rgba(140,165,196,0.55)", rmsTop: "rgba(150,175,205,0.65)", rmsMid: "rgba(190,210,234,0.95)", glow: "rgba(140,170,210,0.0)" };
const AMBER: Palette = { edge: "rgba(255,214,150,0.95)", peakTop: "rgba(255,150,40,0.30)", peakMid: "rgba(255,176,72,0.60)", rmsTop: "rgba(255,166,60,0.72)", rmsMid: "rgba(255,206,132,0.96)", glow: "rgba(255,176,72,0.35)" };

function amp(v: number): number {
  if (v <= 0) return 0;
  const d = 20 * Math.log10(v / 255);
  return Math.max(0, Math.min(1, (d + DB_RANGE) / DB_RANGE));
}
const AMP = new Float32Array(256).map((_, i) => amp(i));
const yFor = (db: number) => (db + DB_RANGE) / DB_RANGE;

export class WaveView {
  private ctx: CanvasRenderingContext2D;
  private w = 0;
  private hgt = 0;
  private dpr = 1;
  private take: Pyramid[] = [];
  private live: Ring[] = [];
  private names: string[] = [];
  private bps = 100;
  private recording = false;
  private stoppedAt = 0;
  private lastTakeBins = 0;
  private hold: { v: number; t: number }[] = [];
  private clip: boolean[] = [];
  private level: number[] = [];
  private dirty = true;
  private tmp = new Float32Array(2);
  private cols = new Float32Array(0);

  constructor(private canvas: HTMLCanvasElement) {
    this.ctx = canvas.getContext("2d", { alpha: true }) as CanvasRenderingContext2D;
    const resize = () => { this.resize(); };
    if ("ResizeObserver" in window) new (window as any).ResizeObserver(resize).observe(canvas);
    window.addEventListener("resize", resize);
    this.resize();
    canvas.addEventListener("click", () => { this.clip = this.clip.map(() => false); this.dirty = true; });
    const loop = () => {
      if (this.dirty || (this.stoppedAt && performance.now() - this.stoppedAt < 1200)) this.draw();
      requestAnimationFrame(loop);
    };
    requestAnimationFrame(loop);
  }

  private resize() {
    const r = this.canvas.getBoundingClientRect();
    this.dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.w = Math.max(1, Math.floor(r.width));
    this.hgt = Math.max(1, Math.floor(r.height));
    this.canvas.width = Math.floor(this.w * this.dpr);
    this.canvas.height = Math.floor(this.hgt * this.dpr);
    this.dirty = true;
  }

  invalidate() {
    this.dirty = true;
  }

  setTracks(names: string[]) {
    if (names.length === this.names.length && names.every((n, i) => n === this.names[i])) return;
    const resetData = names.length !== this.names.length;
    this.names = names.slice();
    if (resetData) {
      this.live = names.map(() => new Ring(this.bps * LIVE_SECONDS));
      if (!this.recording) this.take = names.map(() => new Pyramid());
      this.hold = names.map(() => ({ v: 0, t: 0 }));
      this.clip = names.map(() => false);
      this.level = names.map(() => 0);
    }
    this.dirty = true;
  }

  history(rolling: number[][], take: number[][], recording: boolean, bps: number) {
    this.bps = bps;
    this.live = rolling.map((r) => {
      const ring = new Ring(bps * LIVE_SECONDS);
      for (let i = 0; i + 1 < r.length; i += 2) ring.push(r[i], r[i + 1]);
      return ring;
    });
    if (recording || take.some((t) => t.length)) {
      this.take = take.map((t) => {
        const p = new Pyramid();
        for (let i = 0; i + 1 < t.length; i += 2) p.push(t[i], t[i + 1]);
        return p;
      });
    }
    if (recording && !this.recording) this.clip = this.clip.map(() => false);
    this.recording = recording;
    if (recording) this.stoppedAt = 0;
    this.dirty = true;
  }

  /** bins: per track interleaved (peak, rms). levels: linear peak per track. */
  feed(bins: number[][], levels: number[], recording: boolean) {
    if (recording !== this.recording) {
      if (!recording) {
        this.stoppedAt = performance.now();
        this.lastTakeBins = this.take[0] ? this.take[0].length : 0;
      } else {
        this.take = this.names.map(() => new Pyramid());
        this.clip = this.clip.map(() => false);
        this.stoppedAt = 0;
      }
      this.recording = recording;
    }
    while (this.live.length < bins.length) this.live.push(new Ring(this.bps * LIVE_SECONDS));
    while (this.take.length < bins.length) this.take.push(new Pyramid());
    for (let c = 0; c < bins.length; c++) {
      const b = bins[c];
      for (let i = 0; i + 1 < b.length; i += 2) {
        this.live[c].push(b[i], b[i + 1]);
        if (recording) this.take[c].push(b[i], b[i + 1]);
        if (b[i] >= 254) this.clip[c] = true;
      }
    }
    const now = performance.now();
    for (let c = 0; c < levels.length; c++) {
      const v = levels[c];
      this.level[c] = Math.max(v, (this.level[c] || 0) * 0.82);
      const hd = this.hold[c] || (this.hold[c] = { v: 0, t: 0 });
      if (v >= hd.v || now - hd.t > 1800) { hd.v = v; hd.t = now; }
    }
    this.dirty = true;
  }

  private draw() {
    this.dirty = false;
    const g = this.ctx, W = this.w, H = this.hgt, dpr = this.dpr;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, W, H);
    const n = Math.max(1, this.names.length);
    const RULER = 22;
    const top = RULER, laneH = (H - top) / n;
    const showLast = !this.recording && this.stoppedAt > 0 && this.lastTakeBins > 0 && this.take[0] && this.take[0].length > 0;
    const fade = this.stoppedAt ? Math.min(1, (performance.now() - this.stoppedAt) / 1200) : 1;
    const rec = this.recording;

    // ruler
    const totalBins = rec ? (this.take[0] ? this.take[0].length : 0) : 0;
    const windowBins = rec ? Math.max(totalBins, this.bps * 60) : this.bps * LIVE_SECONDS;
    this.ruler(g, W, RULER, windowBins, rec);

    if (this.cols.length < W * 2) this.cols = new Float32Array(W * 2);
    for (let c = 0; c < n; c++) {
      const y0 = top + c * laneH;
      const mid = y0 + laneH / 2;
      const half = laneH / 2 - Math.min(6, laneH * 0.08);
      // lane separation + guides
      if (c > 0) {
        g.fillStyle = "rgba(0,0,0,0.55)"; g.fillRect(0, Math.round(y0) - 1, W, 1);
        g.fillStyle = "rgba(255,255,255,0.045)"; g.fillRect(0, Math.round(y0), W, 1);
      }
      if (half > 14) {
        for (const db of [-18, -6]) {
          const dy = yFor(db) * half;
          g.fillStyle = db === -6 ? "rgba(255,176,72,0.07)" : "rgba(255,255,255,0.035)";
          g.fillRect(0, Math.round(mid - dy), W, 1);
          g.fillRect(0, Math.round(mid + dy), W, 1);
        }
      }
      g.fillStyle = "rgba(255,255,255,0.06)";
      g.fillRect(0, Math.round(mid), W, 1);

      // columns
      const cols = this.cols;
      const pal = rec ? AMBER : STEEL;
      let alpha = 1;
      if (rec) {
        const t = this.take[c];
        if (t) for (let x = 0; x < W; x++) {
          const a = (x / W) * windowBins, b = ((x + 1) / W) * windowBins;
          if (a >= t.length) { cols[x * 2] = -1; continue; }
          t.query(a, b, this.tmp);
          cols[x * 2] = AMP[this.tmp[0]]; cols[x * 2 + 1] = AMP[this.tmp[1]];
        }
      } else {
        const r = this.live[c];
        const cap = this.bps * LIVE_SECONDS;
        if (r) for (let x = 0; x < W; x++) {
          const a = Math.floor((x / W) * cap), b = Math.max(a + 1, Math.floor(((x + 1) / W) * cap));
          let pk = 0, rm = 0;
          for (let i = a; i < b; i++) { r.at(i, this.tmp); if (this.tmp[0] > pk) pk = this.tmp[0]; if (this.tmp[1] > rm) rm = this.tmp[1]; }
          cols[x * 2] = AMP[pk]; cols[x * 2 + 1] = AMP[rm];
        }
        alpha = showLast ? fade : 1;
      }
      if (showLast && fade < 1) {
        // the take you just played gently dissolves into the live input
        g.globalAlpha = 1 - fade;
        this.drawTakeFrozen(g, c, W, mid, half);
        g.globalAlpha = 1;
      }
      g.globalAlpha = alpha;
      this.shape(g, cols, W, mid, half, pal);
      g.globalAlpha = 1;
      // clipping ticks
      if (rec || !showLast) {
        g.fillStyle = "rgba(255,80,60,0.95)";
        for (let x = 0; x < W; x++) if (cols[x * 2] >= 0.999) { g.fillRect(x, mid - half - 2, 1, 4); g.fillRect(x, mid + half - 2, 1, 4); }
      }
      this.laneLabel(g, c, y0, laneH);
    }

    // record head
    if (rec && totalBins > 0) {
      const x = Math.min(W - 1, (totalBins / windowBins) * W);
      const grd = g.createLinearGradient(x - 40, 0, x, 0);
      grd.addColorStop(0, "rgba(255,70,50,0)");
      grd.addColorStop(1, "rgba(255,70,50,0.16)");
      g.fillStyle = grd; g.fillRect(x - 40, top, 40, H - top);
      g.fillStyle = "rgba(255,96,70,0.95)"; g.fillRect(Math.round(x), top, 1.5, H - top);
      g.beginPath(); g.moveTo(x - 5, top); g.lineTo(x + 6, top); g.lineTo(x + 0.5, top + 7); g.closePath(); g.fill();
    } else if (!rec) {
      // "now" edge
      g.fillStyle = "rgba(170,195,225,0.35)"; g.fillRect(W - 1.5, top, 1.5, H - top);
    }
  }

  private drawTakeFrozen(g: CanvasRenderingContext2D, c: number, W: number, mid: number, half: number) {
    const t = this.take[c];
    if (!t) return;
    const cols = new Float32Array(W * 2);
    const win = Math.max(this.lastTakeBins, this.bps * 60);
    for (let x = 0; x < W; x++) {
      const a = (x / W) * win, b = ((x + 1) / W) * win;
      if (a >= t.length) { cols[x * 2] = -1; continue; }
      t.query(a, b, this.tmp);
      cols[x * 2] = AMP[this.tmp[0]]; cols[x * 2 + 1] = AMP[this.tmp[1]];
    }
    this.shape(g, cols, W, mid, half, AMBER);
  }

  private shape(g: CanvasRenderingContext2D, cols: Float32Array, W: number, mid: number, half: number, p: Palette) {
    let end = W;
    while (end > 0 && cols[(end - 1) * 2] < 0) end--;
    if (end < 2) return;
    const path = (k: number) => {
      g.beginPath();
      g.moveTo(0, mid - Math.max(0.5, cols[k] * half));
      for (let x = 1; x < end; x++) g.lineTo(x + 0.5, mid - Math.max(0.5, cols[x * 2 + k] * half));
      for (let x = end - 1; x >= 0; x--) g.lineTo(x + 0.5, mid + Math.max(0.5, cols[x * 2 + k] * half));
      g.closePath();
    };
    const grad = (a: string, b: string) => {
      const gr = g.createLinearGradient(0, mid - half, 0, mid + half);
      gr.addColorStop(0, a); gr.addColorStop(0.5, b); gr.addColorStop(1, a);
      return gr;
    };
    path(0);
    g.fillStyle = grad(p.peakTop, p.peakMid);
    g.fill();
    path(1);
    g.fillStyle = grad(p.rmsTop, p.rmsMid);
    g.fill();
    // bright edge on the peak envelope
    g.beginPath();
    g.moveTo(0, mid - cols[0] * half);
    for (let x = 1; x < end; x++) g.lineTo(x + 0.5, mid - cols[x * 2] * half);
    g.strokeStyle = p.edge; g.lineWidth = 1; g.globalCompositeOperation = "lighter";
    g.globalAlpha *= 0.45; g.stroke(); g.globalAlpha /= 0.45;
    g.globalCompositeOperation = "source-over";
  }

  private ruler(g: CanvasRenderingContext2D, W: number, H: number, windowBins: number, rec: boolean) {
    const secs = windowBins / this.bps;
    const steps = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600];
    const pxPerSec = W / secs;
    const step = steps.find((s) => s * pxPerSec >= 90) || 3600;
    g.fillStyle = "rgba(0,0,0,0.35)"; g.fillRect(0, 0, W, H);
    g.fillStyle = "rgba(255,255,255,0.05)"; g.fillRect(0, H - 1, W, 1);
    g.font = "600 10px Oswald, 'Roboto Condensed', sans-serif";
    g.textBaseline = "middle";
    for (let s = 0; s <= secs + 0.001; s += step) {
      const x = rec ? s * pxPerSec : W - s * pxPerSec;
      g.fillStyle = "rgba(255,255,255,0.16)"; g.fillRect(Math.round(x), H - 7, 1, 6);
      const sub = step / 5;
      for (let k = 1; k < 5; k++) {
        const xs = rec ? (s + k * sub) * pxPerSec : W - (s + k * sub) * pxPerSec;
        g.fillStyle = "rgba(255,255,255,0.07)"; g.fillRect(Math.round(xs), H - 4, 1, 3);
      }
      if (x > 4 && x < W - 150) {
        const label = rec ? clock(s) : s === 0 ? "now" : "-" + clock(s);
        g.fillStyle = rec ? "rgba(255,190,110,0.62)" : "rgba(190,205,225,0.45)";
        g.textAlign = "left";
        g.fillText(label, Math.round(x) + 4, H / 2 - 1);
      }
    }
    g.textAlign = "right";
    g.fillStyle = rec ? "rgba(255,120,90,0.9)" : "rgba(170,190,215,0.55)";
    g.font = "600 9px Roboto, sans-serif";
    (g as any).letterSpacing = "2px";
    g.fillText(rec ? "● RECORDING" : "INPUT  ·  LAST 30 S", W - 10, H / 2);
    (g as any).letterSpacing = "0px";
  }

  private laneLabel(g: CanvasRenderingContext2D, c: number, y0: number, laneH: number) {
    const name = this.names[c] || "";
    const small = laneH < 54;
    const x = 12, y = y0 + (small ? laneH / 2 : 16);
    const lvl = this.level[c] || 0;
    const hold = this.hold[c] ? this.hold[c].v : 0;
    const db = hold > 0 ? 20 * Math.log10(hold) : -Infinity;
    // plate behind the label
    const pw = small ? 168 : 178, ph = small ? Math.min(22, laneH - 4) : 38;
    const py = small ? y - ph / 2 : y0 + 6;
    g.fillStyle = "rgba(8,7,10,0.72)";
    roundRect(g, x - 6, py, pw, ph, 6); g.fill();
    g.strokeStyle = "rgba(255,255,255,0.05)"; g.lineWidth = 1; g.stroke();
    // clip LED
    const lx = x + 4, ly = small ? y : py + 12;
    ledDot(g, lx, ly, 3.2, this.clip[c] ? "#ff4a32" : "#3a1612", this.clip[c]);
    g.textBaseline = "middle";
    g.textAlign = "left";
    g.font = `500 ${small ? 11 : 12}px Oswald, sans-serif`;
    (g as any).letterSpacing = "1.5px";
    g.fillStyle = this.recording ? "rgba(255,226,180,0.95)" : "rgba(226,222,236,0.9)";
    g.fillText(name.toUpperCase().slice(0, 16), lx + 10, ly + 0.5);
    (g as any).letterSpacing = "0px";
    g.textAlign = "right";
    g.font = "500 10px Roboto, sans-serif";
    g.fillStyle = db > -1 ? "#ff6a4a" : db > -6 ? "#ffc35a" : "rgba(200,196,214,0.6)";
    g.fillText(isFinite(db) ? db.toFixed(1) : "-inf", x + pw - 12, ly + 0.5);
    // meter bar
    if (!small) {
      const mx = x, my = py + 26, mw = pw - 18, mh = 4;
      g.fillStyle = "rgba(255,255,255,0.06)"; g.fillRect(mx, my, mw, mh);
      const a = AMP[Math.min(255, Math.round(lvl * 255))];
      const grd = g.createLinearGradient(mx, 0, mx + mw, 0);
      grd.addColorStop(0, "#5fd38a"); grd.addColorStop(yFor(-12), "#a8e070"); grd.addColorStop(yFor(-6), "#ffc35a"); grd.addColorStop(yFor(-1), "#ff5a3c"); grd.addColorStop(1, "#ff3a2a");
      g.fillStyle = grd; g.fillRect(mx, my, mw * a, mh);
      const hx = mx + mw * AMP[Math.min(255, Math.round(hold * 255))];
      g.fillStyle = "rgba(255,240,220,0.9)"; g.fillRect(hx - 1, my - 1, 1.5, mh + 2);
    }
  }
}

function clock(s: number) {
  s = Math.round(s);
  const m = Math.floor(s / 60), ss = s % 60;
  return m >= 60 ? `${Math.floor(m / 60)}:${String(m % 60).padStart(2, "0")}:${String(ss).padStart(2, "0")}` : `${m}:${String(ss).padStart(2, "0")}`;
}

function roundRect(g: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, r: number) {
  g.beginPath();
  g.moveTo(x + r, y);
  g.arcTo(x + w, y, x + w, y + h, r);
  g.arcTo(x + w, y + h, x, y + h, r);
  g.arcTo(x, y + h, x, y, r);
  g.arcTo(x, y, x + w, y, r);
  g.closePath();
}

function ledDot(g: CanvasRenderingContext2D, x: number, y: number, r: number, col: string, lit: boolean) {
  if (lit) {
    const halo = g.createRadialGradient(x, y, 0, x, y, r * 4);
    halo.addColorStop(0, "rgba(255,70,40,0.45)"); halo.addColorStop(1, "rgba(255,70,40,0)");
    g.fillStyle = halo; g.beginPath(); g.arc(x, y, r * 4, 0, Math.PI * 2); g.fill();
  }
  g.fillStyle = "rgba(0,0,0,0.7)"; g.beginPath(); g.arc(x, y, r + 1, 0, Math.PI * 2); g.fill();
  g.fillStyle = col; g.beginPath(); g.arc(x, y, r, 0, Math.PI * 2); g.fill();
  g.fillStyle = "rgba(255,255,255,0.3)"; g.beginPath(); g.arc(x - r * 0.3, y - r * 0.35, r * 0.35, 0, Math.PI * 2); g.fill();
}
