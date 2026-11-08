// Tiny DOM helpers. No framework: the UI is small and has to run on old iPads.

type Attrs = Record<string, string | number | boolean | ((e: any) => void) | undefined | null>;
type Child = Node | string | number | null | undefined | false;

export function h<K extends keyof HTMLElementTagNameMap>(tag: K, attrs?: Attrs | null, ...children: (Child | Child[])[]): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (attrs) {
    for (const k in attrs) {
      const v = attrs[k];
      if (v === undefined || v === null || v === false) continue;
      if (k.startsWith("on") && typeof v === "function") el.addEventListener(k.slice(2), v as EventListener);
      else if (k === "class") el.className = String(v);
      else if (k === "html") el.innerHTML = String(v);
      else if (k in el && typeof v !== "string") (el as any)[k] = v;
      else el.setAttribute(k, v === true ? "" : String(v));
    }
  }
  append(el, children);
  return el;
}

function append(el: Node, children: (Child | Child[])[]) {
  for (const c of children) {
    if (Array.isArray(c)) append(el, c);
    else if (c !== null && c !== undefined && c !== false) el.appendChild(typeof c === "object" ? c : document.createTextNode(String(c)));
  }
}

export function setText(el: Element, t: string) {
  if (el.textContent !== t) el.textContent = t;
}

export function toggleClass(el: Element, c: string, on: boolean) {
  if (el.classList.contains(c) !== on) el.classList.toggle(c, on);
}

export function clear(el: Element) {
  while (el.firstChild) el.removeChild(el.firstChild);
}

/** Hardware LED. */
export function led(color: "green" | "amber" | "red" | "off" | "steel" = "off", extra = ""): HTMLElement {
  return h("span", { class: `led led-${color} ${extra}` });
}

export function setLed(el: Element, color: string, blink = false) {
  const c = `led led-${color}${blink ? " blink" : ""}${el.classList.contains("led-lg") ? " led-lg" : ""}${el.classList.contains("led-sm") ? " led-sm" : ""}`;
  if (el.className !== c) el.className = c;
}

export function fmtBytes(b: number): string {
  if (!b) return "-";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (b >= 1024 && i < u.length - 1) { b /= 1024; i++; }
  return `${b < 10 && i > 2 ? b.toFixed(1) : Math.round(b)} ${u[i]}`;
}

export function fmtClock(sec: number, tenths = false): string {
  sec = Math.max(0, sec);
  const hh = Math.floor(sec / 3600), mm = Math.floor(sec / 60) % 60, ss = Math.floor(sec) % 60;
  const p = (n: number) => (n < 10 ? "0" : "") + n;
  return `${p(hh)}:${p(mm)}:${p(ss)}${tenths ? "." + Math.floor((sec * 10) % 10) : ""}`;
}

export function fmtDur(sec: number): string {
  const s = Math.round(sec);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor(s / 60) % 60).padStart(2, "0")}m`;
}

export function dB(lin: number): number {
  return lin > 0 ? 20 * Math.log10(lin) : -Infinity;
}

/** Buttons must never keep focus, or SPACE would "click" them. */
export function noFocus(el: HTMLElement): HTMLElement {
  el.setAttribute("tabindex", "-1");
  el.addEventListener("mousedown", (e) => e.preventDefault());
  return el;
}
