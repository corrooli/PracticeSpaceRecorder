// Preset editor + setup sheet. Lives on the recording computer only.

import { h, clear, led, setLed, setText, toggleClass, noFocus, fmtBytes } from "./dom";
import type { AppState, Preset, Destination, DestStatus, Mode, Format, LiveMsg, DestTest, ChatList, Chat, Encoding } from "./types";
import { KINDS, BITRATES, kindInfo, newDestination, parseShareUrl, bytesPerMinute } from "./destinations";

export interface EditorApi {
  send(cmd: Record<string, unknown>): void;
  pickFolder(start: string): Promise<string | null>;
  testDest(d: Destination): Promise<DestTest>;
  findChats(d: Destination): Promise<ChatList>;
  onLive(f: (m: LiveMsg) => void): () => void;
  getMeters(): number[];
  isPrivileged(): boolean;
}

type Tab = "presets" | "setup";
const clone = <T>(x: T): T => JSON.parse(JSON.stringify(x));
const uid = (p: string) => p + Date.now().toString(36) + Math.random().toString(36).slice(2, 6);

function key(label: string, onClick: () => void, cls = ""): HTMLButtonElement {
  const b = h("button", { class: "key " + cls }, h("i", { class: "win" }), h("span", null, label)) as HTMLButtonElement;
  noFocus(b);
  b.addEventListener("click", onClick);
  return b;
}

function seg<T extends string>(opts: [T, string][], value: T, onChange: (v: T) => void): HTMLElement {
  const wrap = h("div", { class: "seg" });
  for (const [v, label] of opts) {
    const b = noFocus(h("button", { class: v === value ? "on" : "" }, label));
    b.addEventListener("click", () => {
      Array.prototype.forEach.call(wrap.children, (c: Element) => c.classList.remove("on"));
      b.classList.add("on");
      onChange(v);
    });
    wrap.appendChild(b);
  }
  return wrap;
}

function toggle(label: string, sub: string, on: boolean, onChange: (v: boolean) => void): HTMLElement {
  const el = h("div", { class: "toggle" + (on ? " on" : "") }, h("span", { class: "sw" }), h("div", null, h("b", null, label), sub ? h("small", null, sub) : null));
  el.addEventListener("click", () => {
    const v = !el.classList.contains("on");
    toggleClass(el, "on", v);
    onChange(v);
  });
  return el;
}

function field(label: string, control: HTMLElement, hint?: string | HTMLElement): HTMLElement {
  return h("div", { class: "field" }, h("label", { class: "engraved" }, label), control, hint ? h("div", { class: "hint" }, hint) : null);
}

function input(value: string, onInput: (v: string) => void, attrs: Record<string, string> = {}): HTMLInputElement {
  const el = h("input", { class: "input", value, spellcheck: "false", autocomplete: "off", ...attrs }) as HTMLInputElement;
  el.value = value;
  el.addEventListener("input", () => onInput(el.value));
  return el;
}

export function expandTemplate(tpl: string, preset: string, take: number, d = new Date()): string {
  const p = (n: number) => String(n).padStart(2, "0");
  const date = `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  const time = `${p(d.getHours())}-${p(d.getMinutes())}-${p(d.getSeconds())}`;
  const clean = (s: string) => s.replace(/[\\/:*?"<>|]/g, "-").trim();
  return (tpl.trim() || "{date} {time} {preset}")
    .split(/[\\/]/)
    .map((part) => part.split("{preset}").join(clean(preset)).split("{date}").join(date).split("{time}").join(time)
      .split("{take}").join(p(take)).split("{year}").join(String(d.getFullYear())).split("{month}").join(p(d.getMonth() + 1)).split("{day}").join(p(d.getDate())))
    .map(clean)
    .filter((x) => x)
    .join(" / ");
}

export class Editor {
  private root = document.getElementById("sheet") as HTMLElement;
  private tab: Tab = "presets";
  private state: AppState | null = null;
  private selId = "";
  private draft: Preset | null = null;
  private dirty = false;
  private body: HTMLElement | null = null;
  private listEl: HTMLElement | null = null;
  private footHint: HTMLElement | null = null;
  private saveBtn: HTMLButtonElement | null = null;
  private chMeters: { ch: number; bar: HTMLElement }[] = [];
  /** `hold`: a TEST result is on screen until then; live status waits. */
  private destLeds: { id: string; led: HTMLElement; text: HTMLElement; card: HTMLElement; lands: HTMLElement | null; hold: number }[] = [];
  /** The "where should takes go?" tiles are open. */
  private picking = false;
  private devSig = "";
  private unLive: (() => void) | null = null;
  private api!: EditorApi;

  init(api: EditorApi) {
    this.api = api;
    this.root.addEventListener("mousedown", (e) => { if (e.target === this.root) this.close(); });
  }

  isOpen() { return !this.root.classList.contains("hidden"); }

  open(tab: Tab) {
    if (!this.api.isPrivileged() || !this.state) return;
    this.tab = tab;
    if (!this.draft || !this.dirty) this.select(this.state.active);
    this.build();
    toggleClass(this.root, "hidden", false);
    if (!this.unLive) this.unLive = this.api.onLive(() => this.meters());
  }

  close() {
    if (!this.isOpen()) return;
    if (this.dirty) {
      this.flash("Unsaved changes. SAVE them, or REVERT first.");
      return;
    }
    toggleClass(this.root, "hidden", true);
    if (this.unLive) { this.unLive(); this.unLive = null; }
  }

  update(s: AppState) {
    const prev = this.state;
    this.state = s;
    if (!this.isOpen()) return;
    // preset list (names / active) and statuses refresh live; the form stays put while you type.
    this.renderList();
    for (const d of this.destLeds) {
      const st = s.dests.find((x) => x.id === d.id);
      if (this.draft && this.draft.id === s.active && st && Date.now() >= d.hold && d.card.dataset.state !== "off") {
        this.showStatus(d, st);
      }
    }
    const sig = s.devices.map((d) => d.name + d.channels).join("|");
    if (sig !== this.devSig && this.tab === "presets" && !this.typing()) this.renderForm();
    if (this.tab === "setup" && prev && (prev.remote.clients !== s.remote.clients || prev.remote.urls[0] !== s.remote.urls[0])) this.build();
    if (this.draft && !s.presets.some((p) => p.id === this.draft!.id) && !this.dirty && this.draft.id) this.select(s.active), this.build();
  }

  private typing() {
    const a = document.activeElement as HTMLElement | null;
    return !!a && this.root.contains(a) && (a.tagName === "INPUT" || a.tagName === "SELECT");
  }

  private select(id: string) {
    const s = this.state!;
    const p = s.presets.find((x) => x.id === id) || s.presets[0];
    this.selId = p.id;
    this.draft = clone(p);
    this.dirty = false;
    this.picking = false;
  }

  private setDirty() {
    this.dirty = true;
    if (this.saveBtn) this.saveBtn.classList.add("on");
    if (this.footHint) { setText(this.footHint, "Unsaved changes"); this.footHint.style.color = "var(--amber)"; }
    this.renderList();
  }

  private flash(msg: string) {
    if (!this.footHint) return;
    setText(this.footHint, msg);
    this.footHint.style.color = "#ff9a84";
    if (this.saveBtn) { this.saveBtn.classList.remove("pulse"); void this.saveBtn.offsetWidth; this.saveBtn.classList.add("pulse"); }
  }

  // ─── layout ────────────────────────────────────────────────────────────
  private build() {
    clear(this.root);
    const s = this.state!;
    const tabs = h("div", { class: "tabs" },
      key("PRESETS", () => { this.tab = "presets"; this.build(); }, this.tab === "presets" ? "on" : ""),
      key("SETUP", () => { this.tab = "setup"; this.build(); }, this.tab === "setup" ? "on" : ""),
      key("✕", () => this.close(), "small"));
    const head = h("div", { class: "sheet-head" }, h("div", { class: "sheet-title" }, this.tab === "presets" ? "PRESETS" : "SETUP", " ", h("span", null, "·"), " ", h("span", { class: "engraved dim", style: "letter-spacing:.24em" }, `v${s.version}`)), tabs);
    this.footHint = h("div", { class: "foot-hint" });
    let body: HTMLElement, foot: HTMLElement;
    if (this.tab === "presets") {
      this.listEl = h("div", null);
      const listCol = h("div", { class: "plist" }, h("div", { class: "engraved", style: "display:block;margin:2px 4px 12px" }, "STORED PRESETS"), this.listEl,
        h("div", { class: "plist-actions" }, key("+ NEW", () => this.newPreset()), key("COPY", () => this.duplicate())));
      this.body = h("div", { class: "form" });
      body = h("div", { class: "sheet-body" }, listCol, this.body);
      this.saveBtn = key("SAVE", () => this.save(false), this.dirty ? "on" : "");
      foot = h("div", { class: "sheet-foot" }, this.footHint,
        h("div", null, key("DELETE", () => this.remove(), "red"), key("REVERT", () => { this.select(this.selId); this.renderForm(); this.renderList(); setText(this.footHint!, ""); }), this.saveBtn, key("SAVE & USE", () => this.save(true), "")));
      setText(this.footHint, this.dirty ? "Unsaved changes" : "Presets are stored on disk the moment you hit SAVE.");
      this.renderList();
      this.renderForm();
    } else {
      this.body = h("div", { class: "form" });
      body = h("div", { class: "sheet-body single" }, this.body);
      foot = h("div", { class: "sheet-foot" }, this.footHint, h("div", null, key("QUIT APP", () => this.api.send({ cmd: "quit" }), "red"), key("DONE", () => this.close(), "on")));
      setText(this.footHint, "Settings save themselves.");
      this.renderSetup();
    }
    this.root.appendChild(h("div", { class: "sheet-card plate" }, head, body, foot));
  }

  private renderList() {
    if (!this.listEl || !this.state) return;
    const s = this.state;
    clear(this.listEl);
    const list = s.presets.slice();
    if (this.draft && !list.some((p) => p.id === this.draft!.id)) list.push(this.draft);
    for (const p of list) {
      const isSel = p.id === this.selId;
      const name = isSel && this.draft ? this.draft.name : p.name;
      const b = noFocus(h("button", { class: "plist-item" + (isSel ? " sel" : "") }, led(p.id === s.active ? "green" : "off", "led-sm"), name + (isSel && this.dirty ? "  •" : "")));
      b.addEventListener("click", () => {
        if (p.id === this.selId) return;
        if (this.dirty) return this.flash("Unsaved changes. SAVE them, or REVERT first.");
        this.select(p.id);
        this.renderList();
        this.renderForm();
        setText(this.footHint!, "");
      });
      this.listEl.appendChild(b);
    }
  }

  // ─── preset form ───────────────────────────────────────────────────────
  private renderForm() {
    if (!this.body || !this.draft || !this.state) return;
    const s = this.state, p = this.draft;
    this.devSig = s.devices.map((d) => d.name + d.channels).join("|");
    clear(this.body);
    this.chMeters = [];
    this.destLeds = [];
    const set = <K extends keyof Preset>(k: K, v: Preset[K], rerender = false) => {
      (p as any)[k] = v;
      this.setDirty();
      if (rerender) this.renderForm();
    };

    // name + device + rate
    const dev = s.devices.find((d) => d.name === p.device) || (p.device ? null : s.devices.find((d) => d.is_default) || null);
    const devSel = h("select", { class: "select" }) as HTMLSelectElement;
    devSel.appendChild(h("option", { value: "" }, `System default${s.devices.find((d) => d.is_default) ? ` (${s.devices.find((d) => d.is_default)!.name})` : ""}`));
    for (const d of s.devices) devSel.appendChild(h("option", { value: d.name }, `${d.name}  ·  ${d.channels} in`));
    if (p.device && !s.devices.some((d) => d.name === p.device)) devSel.appendChild(h("option", { value: p.device }, `${p.device}  (not connected)`));
    devSel.value = p.device;
    devSel.addEventListener("change", () => set("device", devSel.value, true));
    const rates = dev && dev.rates.length ? dev.rates : [44100, 48000, 88200, 96000];
    const rateSel = h("select", { class: "select" }) as HTMLSelectElement;
    for (const r of rates.indexOf(p.sample_rate) < 0 ? rates.concat([p.sample_rate]) : rates) rateSel.appendChild(h("option", { value: String(r) }, `${r / 1000} kHz`));
    rateSel.value = String(p.sample_rate);
    rateSel.addEventListener("change", () => set("sample_rate", Number(rateSel.value)));
    const nameIn = input(p.name, (v) => { p.name = v; this.setDirty(); });

    this.body.appendChild(h("div", { class: "section" }, h("div", { class: "row three" },
      field("PRESET NAME", nameIn), field("INPUT DEVICE", devSel), field("SAMPLE RATE", rateSel))));

    // mode + channels
    const devCh = dev ? dev.channels : Math.max(2, ...p.channels);
    const modeSeg = seg<Mode>([["mono", "MONO"], ["stereo", "STEREO"], ["multitrack", "MULTITRACK"]], p.mode, (m) => {
      p.mode = m;
      if (m === "mono") p.channels = [p.channels[0] || 1];
      else if (m === "stereo") { const l = p.channels[0] || 1; p.channels = [l, Math.min(devCh, l + 1) === l ? Math.max(1, l - 1) : l + 1]; }
      else p.channels = Array.from({ length: devCh }, (_, i) => i + 1);
      this.setDirty();
      this.renderForm();
    });
    const names = new Map<number, string>();
    p.channels.forEach((c, i) => { if (p.track_names[i]) names.set(c, p.track_names[i]); });
    const setChannels = (chs: number[]) => {
      p.channels = chs;
      p.track_names = chs.map((c) => names.get(c) || "");
      this.setDirty();
      this.renderForm();
    };
    const grid = h("div", { class: "chgrid" });
    for (let c = 1; c <= devCh; c++) {
      const idx = p.channels.indexOf(c);
      const on = idx >= 0;
      const sub = !on ? "IN" : p.mode === "stereo" ? (idx === 0 ? "LEFT" : "RIGHT") : p.mode === "mono" ? "MONO" : `TRK ${idx + 1}`;
      const bar = h("i");
      const b = noFocus(h("button", { class: "ch" + (on ? " on" : "") }, String(c), h("small", null, sub), h("span", { class: "mtr" }, bar)));
      this.chMeters.push({ ch: c - 1, bar });
      b.addEventListener("click", (e: MouseEvent) => {
        if (p.mode === "mono") setChannels([c]);
        else if (p.mode === "stereo") {
          if (e.altKey) setChannels([p.channels[0] || 1, c]);
          else setChannels(c < devCh ? [c, c + 1] : [Math.max(1, c - 1), c]);
        } else setChannels(on ? p.channels.filter((x) => x !== c) : p.channels.concat([c]).sort((a, b) => a - b));
      });
      grid.appendChild(b);
    }
    const tools = h("div", { class: "chtools" }, modeSeg);
    if (p.mode === "multitrack") {
      tools.appendChild(h("span", { style: "width:14px" }));
      tools.appendChild(key("ALL", () => setChannels(Array.from({ length: devCh }, (_, i) => i + 1))));
      tools.appendChild(key("NONE", () => setChannels([])));
    }
    const missing = p.channels.filter((c) => c > devCh);
    if (missing.length && dev) tools.appendChild(h("span", { class: "hint", style: "color:#ff9a84" }, `${dev.name} has ${devCh} input${devCh > 1 ? "s" : ""}. Channel ${missing.join(", ")} will fall back to the last one.`));
    else tools.appendChild(h("span", { class: "hint" }, p.mode === "stereo" ? "Click a channel to pick the pair. Alt-click picks the right channel on its own." : p.mode === "mono" ? "Play something and watch the meters to find your channel." : "Click channels to add or remove tracks. Meters move when you play."));
    const nameRow = h("div", { class: "names" });
    {
      p.channels.forEach((c, i) => {
        const def = p.mode === "stereo" ? (i === 0 ? "Left" : "Right") : p.mode === "mono" ? "Mono" : `In ${c}`;
        const el = input(p.track_names[i] || "", (v) => { while (p.track_names.length < p.channels.length) p.track_names.push(""); p.track_names[i] = v; names.set(c, v); this.setDirty(); }, { placeholder: def });
        nameRow.appendChild(h("div", { class: "nm" }, h("span", null, String(c)), el));
      });
    }
    this.body.appendChild(h("div", { class: "section" }, h("span", { class: "engraved" }, `RECORDING MODE & CHANNELS  ·  ${dev ? dev.name : p.device || "default device"}`), tools, grid,
      p.channels.length ? h("div", { class: "engraved", style: "display:block;margin:16px 0 0" }, "TRACK NAMES") : null, nameRow));

    // format
    const fmtRow = h("div", { class: "row" },
      field("FILE FORMAT", seg<Format>([["wav16", "16-BIT"], ["wav24", "24-BIT"], ["wav32f", "32 FLOAT"]], p.format, (f) => set("format", f)), "WAV. Big files go RF64 automatically, so nothing breaks at 4 GB."),
      field("PRE-ROLL", seg<string>([["0", "OFF"], ["1", "1 S"], ["2", "2 S"], ["5", "5 S"]], String(p.preroll_secs), (v) => set("preroll_secs", Number(v))), "Audio from before you pressed SPACE. Nobody hits the key exactly on the one."));
    this.body.appendChild(h("div", { class: "section" }, fmtRow));
    if (p.mode === "multitrack") {
      this.body.appendChild(h("div", { class: "section" }, toggle("One file per track", "Writes a folder of mono WAVs (01 Kick.wav, 02 Snare.wav…). Drag the folder into any DAW.", p.split_tracks, (v) => set("split_tracks", v, true))));
    }

    // filename
    const preview = h("div", { class: "preview" });
    const updPreview = () => setText(preview, expandTemplate(p.filename, p.name, 1) + (p.mode === "multitrack" && p.split_tracks ? " / 01 …wav" : ".wav"));
    const fnIn = input(p.filename, (v) => { p.filename = v; this.setDirty(); updPreview(); }, { class: "input mono" });
    const chips = h("div", { class: "hint" }, "Tokens: ", ...["{date}", "{time}", "{take}", "{preset}", "{year}", "{month}", "{day}"].map((t) => {
      const c = h("code", { style: "cursor:pointer;margin-right:8px" }, t);
      c.addEventListener("mousedown", (e: Event) => { e.preventDefault(); const pos = fnIn.selectionStart ?? fnIn.value.length; fnIn.value = fnIn.value.slice(0, pos) + t + fnIn.value.slice(pos); fnIn.dispatchEvent(new Event("input")); });
      return c;
    }), "  ·  use / for folders");
    updPreview();
    this.body.appendChild(h("div", { class: "section" }, field("FILE NAME", fnIn, chips), preview));

    // destinations
    const cards = h("div", { class: "dcards" }, p.destinations.map((d, i) => this.destCard(p, d, i)));
    const add = this.picking || !p.destinations.length ? this.kindPicker(p) : (() => {
      const b = noFocus(h("button", { class: "dadd" }, h("span", { class: "dadd-plus" }, "+"), "ADD DESTINATION"));
      b.addEventListener("click", () => { this.picking = true; this.renderForm(); });
      return b;
    })();
    this.body.appendChild(h("div", { class: "section" }, h("span", { class: "engraved" }, "DESTINATIONS  ·  EVERY TAKE GOES TO ALL OF THESE, IN THE BACKGROUND"), cards, add));
    this.meters();
  }

  private kindPicker(p: Preset): HTMLElement {
    const tiles = KINDS.map((k) => {
      const t = noFocus(h("button", { class: `ktile kind-${k.kind}` }, h("i", { class: "win" }),
        h("div", { class: "ktile-icon", html: k.icon }), h("div", { class: "ktile-title" }, k.title), h("div", { class: "ktile-blurb" }, k.blurb)));
      t.addEventListener("click", () => {
        p.destinations.push(newDestination(k.kind, p.destinations, uid("d")));
        this.picking = false;
        this.setDirty();
        this.renderForm();
      });
      return t;
    });
    const cancel = p.destinations.length ? key("CANCEL", () => { this.picking = false; this.renderForm(); }) : null;
    return h("div", { class: "kpicker" },
      h("div", { class: "kpicker-head" }, h("span", { class: "engraved" }, "WHERE SHOULD TAKES GO?"), cancel),
      h("div", { class: "ktiles" }, tiles),
      p.destinations.length ? null : h("div", { class: "hint", style: "margin-top:12px" }, "Nothing picked yet, so takes go to your Music folder for now."));
  }

  /** LED + text + card glow for a live destination status. */
  private showStatus(e: { led: HTMLElement; text: HTMLElement; card: HTMLElement; lands: HTMLElement | null }, st: DestStatus) {
    setLed(e.led, st.state === "ready" ? "green" : st.state === "error" ? "red" : "amber", st.state === "checking" || st.state === "mounting");
    setText(e.text, st.state === "ready" ? (st.free_bytes > 0 ? `Ready · ${fmtBytes(st.free_bytes)} free` : st.message || "Ready") : st.message);
    e.text.className = "dpill-text" + (st.state === "error" ? " err" : "");
    e.card.dataset.state = st.state;
    if (e.lands) {
      toggleClass(e.lands, "hidden", !st.path);
      setText(e.lands.querySelector("b")!, st.path);
    }
  }

  private destCard(p: Preset, d: Destination, i: number): HTMLElement {
    const s = this.state!;
    const info = kindInfo(d.kind);
    const active = p.id === s.active;
    const st = active ? s.dests.find((x) => x.id === d.id) : undefined;
    const l = led("off", "led-sm");
    const text = h("span", { class: "dpill-text" });
    const card = h("div", { class: `dcard kind-${d.kind}` });
    const lands = d.kind === "share" ? h("div", { class: "lands hidden" }, "Lands in ", h("b")) : null;
    const entry = { id: d.id, led: l, text, card, lands, hold: 0 };
    this.destLeds.push(entry);
    if (!d.enabled) { setText(text, "Off"); card.dataset.state = "off"; }
    else if (st) this.showStatus(entry, st);
    else { setText(text, active ? "Checking…" : "Checked when this preset is in use"); card.dataset.state = active ? "checking" : "idle"; if (active) setLed(l, "amber", true); }

    const name = input(d.label, (v) => { d.label = v; this.setDirty(); }, { class: "dname", placeholder: info.label });
    const power = noFocus(h("button", { class: "dswitch" + (d.enabled ? " on" : ""), title: d.enabled ? "On: takes go here (click to pause)" : "Off: skipped (click to turn on)" }));
    power.addEventListener("click", () => { d.enabled = !d.enabled; this.setDirty(); this.renderForm(); });
    const test: HTMLButtonElement = key("TEST", async () => {
      if (test.classList.contains("testing")) return;
      entry.hold = Infinity;
      test.className = "key test testing";
      setLed(l, "amber", true);
      setText(text, "Testing: writing a file and reading it back…");
      text.className = "dpill-text";
      card.dataset.state = "checking";
      const r = await this.api.testDest(clone(d));
      entry.hold = Date.now() + 15000;
      test.className = "key test " + (r.ok ? "ok" : "fail");
      setLed(l, r.ok ? "green" : "red");
      setText(text, r.ok ? (r.free > 0 ? `Works · ${fmtBytes(r.free)} free · ${r.path}` : `Works · ${r.path}`) : r.msg);
      text.className = "dpill-text " + (r.ok ? "good" : "err");
      card.dataset.state = r.ok ? "ready" : "error";
    }, "test");
    test.title = d.kind === "telegram" ? "Say hi in the chat right now" : "Write a test file there right now (connects the share if needed)";
    const remove = key("REMOVE", () => { p.destinations.splice(i, 1); this.setDirty(); this.renderForm(); }, "red");
    remove.title = "Remove this destination (REVERT brings it back)";

    card.append(
      h("div", { class: "dcard-head" },
        h("div", { class: "dicon", html: info.icon }),
        h("div", { class: "dtitle" }, name, h("div", { class: "dmeta" }, h("span", { class: "dkind" }, info.title), h("span", { class: "dpill" }, l, text))),
        h("div", { class: "dactions" }, power, test, remove)),
      h("div", { class: "dcard-body" }, this.destBody(p, d, lands)));
    return card;
  }

  /** The fields that make each kind of destination. */
  private destBody(p: Preset, d: Destination, lands: HTMLElement | null): (HTMLElement | null)[] {
    const s = this.state!;
    const mac = s.platform === "macos";
    const cmdIn = input(d.mount_cmd, (v) => { d.mount_cmd = v; this.setDirty(); }, { class: "input mono", placeholder: "mount /mnt/nas" });
    const advanced = (extra: HTMLElement | null, open: boolean) => {
      const adv = h("div", { class: "adv", style: open ? "" : "display:none" }, field("MOUNT COMMAND (OPTIONAL)", cmdIn, "Runs whenever the folder is missing. Handy on Linux with an fstab entry."), extra);
      const link = noFocus(h("button", { class: "linkish", style: open ? "display:none" : "" }, "Advanced…"));
      link.addEventListener("click", () => { adv.style.display = ""; link.style.display = "none"; });
      return [link, adv];
    };
    if (d.kind === "folder") {
      const pathIn = input(d.path, (v) => { d.path = v; this.setDirty(); }, { class: "input mono", placeholder: mac ? "/Volumes/USB/Band" : "/media/usb/band" });
      const browse = key("BROWSE", async () => {
        const r = await this.api.pickFolder(d.path || s.music_dir);
        if (r) { d.path = r; pathIn.value = r; this.setDirty(); }
      });
      // folders from older versions may still auto-connect a share first
      const legacy = d.smb_url ? field("CONNECT THIS SHARE FIRST", input(d.smb_url, (v) => { d.smb_url = v; this.setDirty(); }, { class: "input mono" })) : null;
      return [field("FOLDER", h("div", { class: "dest-path" }, pathIn, browse), "A missing last folder is created for you."), this.formatRow(p, d), ...advanced(legacy, !!(d.mount_cmd || d.smb_url))];
    }
    if (d.kind === "share") {
      const crumbs = h("div", { class: "crumbs" });
      const draw = () => {
        clear(crumbs);
        const u = parseShareUrl(d.smb_url);
        if (!d.smb_url.trim()) crumbs.append(h("span", { class: "crumb-empty" }, "SERVER  ›  SHARE  ›  FOLDER"));
        else if (!u) crumbs.append(h("span", { class: "crumb-bad" }, "Start with smb://, then the server, the share and the folder."));
        else {
          const parts: [string, string][] = [["SERVER", u.host], ["SHARE", u.share], ...u.sub.map((x): [string, string] => ["FOLDER", x])];
          parts.forEach(([cap, v], k) => {
            if (k) crumbs.append(h("span", { class: "crumb-sep" }, "›"));
            crumbs.append(h("span", { class: "crumb" + (k === 0 ? " host" : "") }, h("small", null, cap), v));
          });
        }
      };
      const urlIn = input(d.smb_url, (v) => { d.smb_url = v; this.setDirty(); draw(); }, { class: "input mono", placeholder: "smb://you@nas.local/Music/Band" });
      draw();
      return [field("SHARE ADDRESS", urlIn, mac ? "Logs in with your Keychain. Takes land in the folder at the end of the address." : "Logs in with your keyring (gio). Takes land in the folder at the end of the address."),
        crumbs, lands, this.formatRow(p, d), ...advanced(null, !!d.mount_cmd)];
    }
    if (d.kind === "telegram") return this.telegramBody(p, d);
    return [h("div", { class: "hint" }, "Made by a newer version of PracticeSpaceRecorder. Kept as it is, but this version can't deliver to it.")];
  }

  /** WAV or MP3 at a bitrate, with an idea of what that costs. */
  private formatRow(p: Preset, d: Destination): HTMLElement {
    const n = p.mode === "mono" ? 1 : p.mode === "stereo" ? 2 : p.channels.length;
    const bits = p.format === "wav16" ? 16 : p.format === "wav24" ? 24 : 32;
    const tg = d.kind === "telegram";
    const hint = h("div", { class: "hint" });
    const rates = seg(BITRATES.map((b): [string, string] => [String(b), String(b)]), String(d.bitrate), (v) => { d.bitrate = Number(v); this.setDirty(); draw(); });
    rates.classList.add("rates");
    const draw = () => {
      const mp3 = d.encoding === "mp3";
      toggleClass(rates, "hidden", !mp3);
      const ch = mp3 || tg ? Math.min(n, 2) : n;
      const per10 = bytesPerMinute(d.encoding, d.bitrate, p.sample_rate, ch, bits) * 10 * (p.split_tracks && !tg && n > 1 ? (mp3 ? n / ch : 1) : 1);
      const notes = [mp3 ? `About ${fmtBytes(per10)} per 10 minutes.` : `As recorded, about ${fmtBytes(per10)} per 10 minutes.`];
      if (mp3 && n > 2 && (tg || !p.split_tracks)) notes.push("More than two tracks become a stereo mix.");
      else if (tg && n > 2) notes.push("Goes out as a stereo mix.");
      if (tg) notes.push(mp3 ? "Telegram takes 50 MB per file: long takes drop to 128 kbps, then come in parts." : "Telegram takes 50 MB per file, so WAV arrives in parts. MP3 saves a lot of data.");
      setText(hint, notes.join(" "));
    };
    const fmt = seg<Encoding>([["wav", "WAV"], ["mp3", "MP3"]], d.encoding, (v) => { d.encoding = v; this.setDirty(); draw(); });
    draw();
    return h("div", { class: "field" }, h("label", { class: "engraved" }, "FORMAT"), h("div", { class: "fmt-row" }, fmt, rates, h("span", { class: "fmt-unit" }, "kbps")), hint);
  }

  /** Bot token (write-only), chat picker, format. */
  private telegramBody(p: Preset, d: Destination): (HTMLElement | null)[] {
    const mask = (t: string) => (t.startsWith("\u2022") || !t ? t : "\u2022\u2022\u2022\u2022" + t.slice(-4));
    const kept = d.token;
    const tokenIn = input("", (v) => { d.token = v.trim() || kept; this.setDirty(); }, { class: "input mono", type: "password", placeholder: mask(d.token) || "123456789:AAH4x…" });
    const steps = !d.token || !d.chat_id
      ? h("ol", { class: "tg-steps" },
        h("li", null, "In Telegram, message ", h("b", null, "@BotFather"), ", send ", h("code", null, "/newbot"), " and copy the token it gives you."),
        h("li", null, "Add your new bot to the band group and say anything in there."),
        h("li", null, "Paste the token below, hit ", h("b", null, "FIND CHATS"), ", pick the group, then ", h("b", null, "TEST"), "."))
      : null;
    const chip = h("div", { class: "chat-chip" });
    const drawChip = () => {
      clear(chip);
      toggleClass(chip, "empty", !d.chat_id);
      if (!d.chat_id) chip.append(h("span", null, "No chat picked yet"));
      else chip.append(h("b", null, d.chat_name || d.chat_id), h("small", null, d.chat_name ? d.chat_id : ""));
    };
    drawChip();
    const list = h("div", { class: "chat-list" });
    const pickChat = (c: Chat) => {
      d.chat_id = c.id;
      d.chat_name = c.name;
      if (!d.label.trim() || d.label === kindInfo("telegram").label) d.label = c.name;
      this.setDirty();
      this.renderForm();
    };
    const KIND: Record<string, string> = { group: "GROUP", supergroup: "GROUP", channel: "CHANNEL", private: "DIRECT" };
    const find: HTMLButtonElement = key("FIND CHATS", async () => {
      if (find.classList.contains("testing")) return;
      find.className = "key test testing";
      clear(list);
      list.append(h("div", { class: "chat-msg" }, "Asking Telegram…"));
      const r = await this.api.findChats(clone(d));
      find.className = "key";
      clear(list);
      if (!r.ok) { list.append(h("div", { class: "chat-msg err" }, r.msg)); return; }
      if (!r.chats.length) { list.append(h("div", { class: "chat-msg" }, `${r.bot} hasn't seen any chats yet. Add it to your band group, say hi in there, then FIND CHATS again.`)); return; }
      list.append(h("div", { class: "chat-msg" }, `Chats ${r.bot} is in. Pick where takes go:`));
      for (const c of r.chats) {
        const b = noFocus(h("button", { class: "chat" + (c.id === d.chat_id ? " on" : "") },
          h("span", { class: "chat-kind" }, KIND[c.kind] || c.kind.toUpperCase()), h("b", null, c.name), h("small", null, c.id)));
        b.addEventListener("click", () => pickChat(c));
        list.append(b);
      }
    });
    const idIn = input(d.chat_id, (v) => { d.chat_id = v.trim(); d.chat_name = ""; this.setDirty(); drawChip(); }, { class: "input mono", placeholder: "-1001234567890 or @channel" });
    const adv = h("div", { class: "adv", style: "display:none" }, field("CHAT ID", idIn, "For channels: add the bot as an admin, then type @channelname here."));
    const link = noFocus(h("button", { class: "linkish" }, "Advanced…"));
    link.addEventListener("click", () => { adv.style.display = ""; link.style.display = "none"; });
    return [steps,
      field("BOT TOKEN", tokenIn, "Kept on this computer only. Once saved it can't be shown or copied again, only replaced."),
      field("CHAT", h("div", { class: "dest-path" }, chip, find)), list,
      this.formatRow(p, d), link, adv];
  }

  private meters() {
    const m = this.api.getMeters();
    for (const { ch, bar } of this.chMeters) {
      const v = m[ch] || 0;
      const db = v > 0 ? 20 * Math.log10(v) : -90;
      const w = Math.max(0, Math.min(100, ((db + 54) / 54) * 100));
      bar.style.width = w.toFixed(1) + "%";
    }
  }

  // ─── preset actions ────────────────────────────────────────────────────
  private save(activate: boolean) {
    if (!this.draft) return;
    const p = this.draft;
    if (p.mode === "multitrack" && !p.channels.length) return this.flash("Pick at least one channel.");
    while (p.track_names.length < p.channels.length) p.track_names.push("");
    p.track_names = p.track_names.slice(0, p.channels.length);
    this.api.send({ cmd: "save_preset", preset: p, activate });
    this.dirty = false;
    if (this.saveBtn) this.saveBtn.classList.remove("on");
    setText(this.footHint!, activate ? "Saved and active." : "Saved.");
    this.footHint!.style.color = "var(--green)";
    this.renderList();
  }

  private newPreset() {
    if (this.dirty) return this.flash("Unsaved changes. SAVE them, or REVERT first.");
    const base = this.state!.presets.find((x) => x.id === this.state!.active)!;
    this.draft = { ...clone(base), id: uid("p"), name: "New Preset" };
    this.draft.destinations = this.draft.destinations.map((d) => ({ ...d, id: uid("d") }));
    this.selId = this.draft.id;
    this.dirty = true;
    this.build();
    this.setDirty();
  }

  private duplicate() {
    if (this.dirty) return this.flash("Unsaved changes. SAVE them, or REVERT first.");
    const src = this.draft!;
    this.draft = { ...clone(src), id: uid("p"), name: src.name + " copy" };
    this.selId = this.draft.id;
    this.build();
    this.setDirty();
  }

  private remove() {
    const s = this.state!;
    if (!this.draft) return;
    if (!s.presets.some((p) => p.id === this.draft!.id)) { this.dirty = false; this.select(s.active); this.build(); return; }
    if (s.presets.length <= 1) return this.flash("You need at least one preset. It's the law.");
    const btn = this.root.querySelector(".sheet-foot .key.red") as HTMLElement;
    if (!btn.classList.contains("on")) {
      btn.classList.add("on");
      btn.querySelector("span")!.textContent = "SURE?";
      setTimeout(() => { btn.classList.remove("on"); const sp = btn.querySelector("span"); if (sp) sp.textContent = "DELETE"; }, 2500);
      return;
    }
    this.api.send({ cmd: "delete_preset", id: this.draft.id });
    this.dirty = false;
    const next = s.presets.find((p) => p.id !== this.draft!.id)!;
    this.select(next.id);
    this.build();
  }

  // ─── setup tab ─────────────────────────────────────────────────────────
  private renderSetup() {
    const s = this.state!, b = this.body!;
    clear(b);
    b.appendChild(h("div", { class: "section" }, h("span", { class: "engraved" }, "WINDOW"),
      toggle("Start in fullscreen", "The app opens straight into fullscreen. Press F to toggle any time.", s.settings.fullscreen, (v) => this.api.send({ cmd: "save_settings", settings: { fullscreen: v } }))));

    const others = s.remote.clients;
    const qr = h("div", { class: "qr", html: s.remote.qr });
    const portIn = input(String(s.settings.port), () => {}, { type: "number", min: "1024", max: "65535", style: "width:120px" });
    portIn.addEventListener("change", () => this.api.send({ cmd: "save_settings", settings: { port: Number(portIn.value) } }));
    b.appendChild(h("div", { class: "section" }, h("span", { class: "engraved" }, "REMOTE CONTROL  ·  TABLET / PHONE"),
      toggle("Allow remote control", "Any device on your network can start, stop and switch presets. Only this computer can edit them.", s.settings.remote_enabled, (v) => this.api.send({ cmd: "save_settings", settings: { remote_enabled: v } })),
      h("div", { class: "remote-box", style: "margin-top:12px" + (s.settings.remote_enabled ? "" : ";opacity:.4") }, qr,
        h("div", null,
          h("div", { class: "engraved", style: "margin-bottom:10px" }, "SCAN, OR OPEN THIS IN SAFARI / CHROME"),
          ...s.remote.urls.map((u, i) => h("div", { class: "url" + (i ? " alt" : "") }, u)),
          h("p", { class: "hint", style: "font-size:13px;line-height:1.5;margin:16px 0" }, "Tip: on an iPad, tap Share, then “Add to Home Screen”. You get a fullscreen remote with a giant REC button, far away from your guitar headstock and the table edge."),
          h("div", { class: "kv" }, h("span", null, "Connected"), h("span", null, others ? `${others} device${others > 1 ? "s" : ""}` : "None right now"), h("span", null, "Port"), h("span", null, portIn))))));

    b.appendChild(h("div", { class: "section" }, h("span", { class: "engraved" }, "HOW YOUR TAKES TRAVEL"),
      h("p", { class: "hint", style: "font-size:13px;line-height:1.6;max-width:780px" },
        "Audio is written to a local buffer first, so a slow or sleeping NAS can never cost you a take. When you stop, the take is delivered to every destination in the background while you already record the next song. If a share is offline, the take waits and delivers itself when the share is back, even after a restart. The buffer copy is deleted only once every destination has it."),
      h("div", { class: "kv", style: "margin-top:10px" }, h("span", null, "Version"), h("span", null, s.version), h("span", null, "Platform"), h("span", null, s.platform), h("span", null, "Default folder"), h("span", null, s.music_dir))));
  }
}
