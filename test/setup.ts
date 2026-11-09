// jsdom has no canvas and no WebSocket server. Give it both, as fakes.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

/** A 2D context that accepts every call and every property, and counts calls. */
export const canvasCalls: Record<string, number> = {};
function fakeContext(): CanvasRenderingContext2D {
  const gradient = { addColorStop() {} };
  const store: Record<string, unknown> = {};
  return new Proxy(store, {
    get(t, k: string) {
      if (k in t) return t[k];
      if (k === "createLinearGradient" || k === "createRadialGradient") return () => gradient;
      return () => { canvasCalls[k] = (canvasCalls[k] || 0) + 1; };
    },
    set(t, k: string, v) { t[k] = v; return true; },
  }) as unknown as CanvasRenderingContext2D;
}
HTMLCanvasElement.prototype.getContext = function () {
  return ((this as any).__ctx ||= fakeContext());
} as any;

export class FakeWS {
  static instances: FakeWS[] = [];
  static last() { return FakeWS.instances[FakeWS.instances.length - 1]; }
  readyState = 0;
  sent: any[] = [];
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((e: { data: string }) => void) | null = null;
  constructor(public url: string) { FakeWS.instances.push(this); }
  send(s: string) { this.sent.push(JSON.parse(s)); }
  open() { this.readyState = 1; this.onopen?.(); }
  close() { this.readyState = 3; this.onclose?.(); }
  msg(m: unknown) { this.onmessage?.({ data: JSON.stringify(m) }); }
  cmds() { return this.sent.map((c) => c.cmd); }
}
(globalThis as any).WebSocket = FakeWS;

/** The real page markup, as the app serves it. */
export function loadPage() {
  const html = readFileSync(resolve(process.cwd(), "index.html"), "utf8");
  document.body.innerHTML = html.slice(html.indexOf("<body>") + 6, html.indexOf("</body>")).replace(/<script[^>]*><\/script>/, "");
}
