import { describe, it, expect, vi, beforeEach } from "vitest";
import { Pyramid, Ring, WaveView } from "../src/wave";
import { canvasCalls } from "./setup";

describe("Pyramid", () => {
  it("answers max queries exactly like a brute force scan", () => {
    const p = new Pyramid();
    const pk: number[] = [], rms: number[] = [];
    let seed = 7;
    const rnd = () => (seed = (seed * 1103515245 + 12345) % 2147483648) % 256;
    for (let i = 0; i < 20000; i++) {
      const a = rnd(), b = rnd();
      pk.push(a); rms.push(b); p.push(a, b);
    }
    expect(p.length).toBe(20000);
    const out = new Float32Array(2);
    for (const [a, b] of [[0, 1], [0, 3], [5, 9], [100, 4196], [0, 20000], [19990, 20000], [1234.5, 5678.9]]) {
      p.query(a, b, out);
      // pyramid levels widen the window to whole blocks; never narrower than [a, b)
      const lo = Math.floor(a), hi = Math.ceil(b);
      expect(out[0]).toBeGreaterThanOrEqual(Math.max(...pk.slice(lo, hi)));
      expect(out[1]).toBeGreaterThanOrEqual(Math.max(...rms.slice(lo, hi)));
    }
    p.query(3, 4, out);
    expect([out[0], out[1]]).toEqual([pk[3], rms[3]]);
    p.query(25000, 26000, out);
    expect([out[0], out[1]]).toEqual([0, 0]);
    p.query(7, 7, out);
    expect(out[0]).toBe(pk[7]);
    p.clear();
    expect(p.length).toBe(0);
  });
});

describe("Ring", () => {
  it("keeps the last cap bins, zero padded at the front", () => {
    const r = new Ring(4);
    const out = new Float32Array(2);
    r.push(1, 2);
    r.at(0, out);
    expect([...out]).toEqual([0, 0]);
    r.at(3, out);
    expect([...out]).toEqual([1, 2]);
    for (let i = 2; i <= 6; i++) r.push(i, i);
    const all = [0, 1, 2, 3].map((i) => (r.at(i, out), out[0]));
    expect(all).toEqual([3, 4, 5, 6]);
    r.clear();
    r.at(3, out);
    expect(out[0]).toBe(0);
  });
});

describe("WaveView", () => {
  let canvas: HTMLCanvasElement;
  let rect = { width: 800, height: 400 };
  beforeEach(() => {
    canvas = document.createElement("canvas");
    canvas.getBoundingClientRect = () => ({ ...rect, x: 0, y: 0, top: 0, left: 0, right: rect.width, bottom: rect.height, toJSON() {} }) as DOMRect;
    document.body.appendChild(canvas);
  });
  const draw = (v: WaveView) => (v as any).draw();
  const bins = (n: number, v: number) => Array.from({ length: n * 2 }, () => v);

  it("sizes the canvas for the device pixel ratio", () => {
    (window as any).devicePixelRatio = 3;
    new WaveView(canvas);
    expect([canvas.width, canvas.height]).toEqual([1600, 800]);
    (window as any).devicePixelRatio = 1;
    rect = { width: 0, height: 0 };
    const v = new WaveView(canvas);
    expect([canvas.width, canvas.height]).toEqual([1, 1]);
    rect = { width: 800, height: 400 };
    window.dispatchEvent(new Event("resize"));
    expect(canvas.width).toBe(800);
    v.invalidate();
    expect((v as any).dirty).toBe(true);
  });

  it("uses ResizeObserver when there is one", () => {
    const observe = vi.fn();
    (window as any).ResizeObserver = class { constructor(public cb: () => void) {} observe = observe; };
    new WaveView(canvas);
    expect(observe).toHaveBeenCalledWith(canvas);
    delete (window as any).ResizeObserver;
  });

  it("live input, then a take, then the fade back", () => {
    const now = vi.spyOn(performance, "now");
    now.mockReturnValue(1000);
    const v = new WaveView(canvas);
    v.setTracks(["Kick", "Snare"]);
    v.setTracks(["Kick", "Snare"]); // no-op
    v.feed([bins(50, 120), bins(50, 255)], [0.5, 1.0], false);
    expect((v as any).clip).toEqual([false, true]);
    draw(v);
    // clicking the bay clears clip LEDs
    canvas.dispatchEvent(new MouseEvent("click"));
    expect((v as any).clip).toEqual([false, false]);
    // recording starts
    v.feed([bins(200, 100), bins(200, 30)], [0.2, 0.1], true);
    expect((v as any).take[0].length).toBe(200);
    draw(v);
    v.setTracks(["Kick", "Snare"]);
    v.setTracks(["Bass", "Drums"]); // renamed mid-take: data kept
    expect((v as any).take[0].length).toBe(200);
    // stop: last take dissolves into live input for 1.2 s
    now.mockReturnValue(2000);
    v.feed([[], []], [0, 0], false);
    expect((v as any).lastTakeBins).toBe(200);
    now.mockReturnValue(2600);
    const before = canvasCalls.stroke || 0;
    draw(v);
    expect((canvasCalls.stroke || 0) - before).toBeGreaterThanOrEqual(4);
    now.mockReturnValue(5000);
    draw(v);
    now.mockRestore();
  });

  it("history replaces buffers and starts a fresh take", () => {
    const v = new WaveView(canvas);
    v.setTracks(["L", "R"]);
    v.history([bins(10, 50), bins(10, 60)], [[], []], false, 100);
    expect((v as any).take[0].length).toBe(0);
    (v as any).clip = [true, true];
    v.history([bins(10, 50), bins(10, 60)], [bins(5, 9), bins(5, 9)], true, 100);
    expect((v as any).take[1].length).toBe(5);
    expect((v as any).clip).toEqual([false, false]);
    expect((v as any).recording).toBe(true);
    draw(v);
    v.history([[], []], [bins(5, 9), bins(5, 9)], false, 100);
    expect((v as any).take[0].length).toBe(5);
    v.history([[1]], [[]], false, 100);
  });

  it("grows buffers when more tracks arrive than were announced", () => {
    const v = new WaveView(canvas);
    v.feed([bins(3, 10), bins(3, 10), bins(3, 10)], [0.1, 0.2, 0.3], true);
    expect((v as any).live.length).toBe(3);
    expect((v as any).take.length).toBe(3);
    draw(v);
  });

  it("peak hold decays after 1.8 s", () => {
    const now = vi.spyOn(performance, "now");
    const v = new WaveView(canvas);
    v.setTracks(["A"]);
    now.mockReturnValue(0);
    v.feed([[]], [0.9], false);
    now.mockReturnValue(1000);
    v.feed([[]], [0.1], false);
    expect((v as any).hold[0].v).toBe(0.9);
    now.mockReturnValue(3000);
    v.feed([[]], [0.1], false);
    expect((v as any).hold[0].v).toBe(0.1);
    now.mockRestore();
  });

  it("draws many small lanes, long takes and level colors", () => {
    const v = new WaveView(canvas);
    const names = Array.from({ length: 16 }, (_, i) => `In ${i + 1} with a very long name`);
    v.setTracks(names);
    // loud, quiet and silent lanes for the dB readout colours
    v.feed(names.map(() => bins(10, 200)), names.map((_, i) => [1, 0.7, 0.1, 0][i % 4]), true);
    draw(v);
    // a 2 hour take: ruler switches to coarse steps
    const big = names.map(() => bins(100 * 7200, 80));
    v.history(names.map(() => []), big, true, 100);
    draw(v);
    // one big lane with guide lines
    v.setTracks(["Mono"]);
    v.feed([bins(5, 128)], [0.95], false);
    draw(v);
    v.setTracks([]);
    draw(v);
  });

  it("redraws from the animation loop only when needed", async () => {
    const v = new WaveView(canvas);
    const spy = vi.spyOn(v as any, "draw");
    await new Promise((r) => setTimeout(r, 50));
    expect(spy).toHaveBeenCalledTimes(1);
    v.invalidate();
    await new Promise((r) => setTimeout(r, 50));
    expect(spy).toHaveBeenCalledTimes(2);
  });
});
