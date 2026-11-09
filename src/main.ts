import "./style.css";
import { h, setText, toggleClass, clear, led, setLed, fmtBytes, fmtClock, fmtDur, noFocus } from "./dom";
import { WaveView } from "./wave";
import { Editor } from "./editor";
import type { AppState, LiveMsg, HistMsg, JobView, Destination, DestTest } from "./types";

// ─── connection ───────────────────────────────────────────────────────────
type Listener = (m: LiveMsg) => void;
let ws: WebSocket | null = null;
let state: AppState | null = null;
let privileged = false;
let liveFrames = 0;
let liveRecording = false;
let lastMeters: number[] = [];
const liveListeners: Listener[] = [];
const pickListeners: Record<string, (p: string | null) => void> = {};
const testListeners: Record<string, (r: DestTest) => void> = {};

function send(cmd: Record<string, unknown>) {
  if (ws && ws.readyState === 1) ws.send(JSON.stringify(cmd));
}
function isPrivileged() { return privileged; }
function getMeters() { return lastMeters; }
function onLive(f: Listener) { liveListeners.push(f); return () => { const i = liveListeners.indexOf(f); if (i >= 0) liveListeners.splice(i, 1); }; }
function pickFolder(start: string): Promise<string | null> {
  const req = Math.random().toString(36).slice(2);
  return new Promise((res) => { pickListeners[req] = res; send({ cmd: "pick_folder", req, start }); });
}

/** Ask the recorder to write a test file where this destination's takes would go. */
function testDest(d: Destination): Promise<DestTest> {
  const req = Math.random().toString(36).slice(2);
  return new Promise((res) => {
    const t = setTimeout(() => { delete testListeners[req]; res({ ok: false, msg: "No answer from the recorder", path: "", free: 0 }); }, 30000);
    testListeners[req] = (r) => { clearTimeout(t); res(r); };
    send({ cmd: "test_dest", req, dest: d });
  });
}

if (/[?&]shot/.test(location.search)) document.body.classList.add("shot");
const $ = (id: string) => document.getElementById(id) as HTMLElement;
const wave = new WaveView($("wave") as HTMLCanvasElement);
const editor = new Editor();
editor.init({ send, pickFolder, testDest, onLive, getMeters, isPrivileged });
let retry = 0;

function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  ws = new WebSocket(`${proto}://${location.host}/ws`);
  ws.onopen = () => { retry = 0; toggleClass($("offline"), "hidden", true); };
  ws.onclose = () => {
    toggleClass($("offline"), "hidden", false);
    setText($("offlineMsg"), retry > 3 ? "Is the recorder app running? Retrying…" : "Reconnecting…");
    setTimeout(connect, Math.min(4000, 400 * ++retry));
  };
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    switch (m.t) {
      case "hello":
        privileged = m.privileged;
        toggleClass(document.body, "remote", !privileged);
        break;
      case "state":
        state = m as AppState;
        render();
        editor.update(state);
        break;
      case "live":
        onLiveMsg(m as LiveMsg);
        break;
      case "hist": {
        const hm = m as HistMsg;
        wave.history(hm.rolling, hm.take, hm.recording, hm.bps);
        break;
      }
      case "picked":
        if (pickListeners[m.req]) { pickListeners[m.req](m.path); delete pickListeners[m.req]; }
        break;
      case "dest_test":
        if (testListeners[m.req]) { testListeners[m.req](m as DestTest); delete testListeners[m.req]; }
        break;
    }
  };
}

function onLiveMsg(m: LiveMsg) {
  lastMeters = m.m.map((v) => v / 1000);
  liveFrames = m.f;
  liveRecording = m.r;
  const sel = state && state.audio ? state.audio.selected : [];
  wave.feed(m.b, sel.map((c) => lastMeters[c] || 0), m.r);
  for (const f of liveListeners) f(m);
  renderClock();
}

// ─── header ───────────────────────────────────────────────────────────────
noFocus($("prevPreset")).addEventListener("click", () => send({ cmd: "step_preset", dir: -1 }));
noFocus($("nextPreset")).addEventListener("click", () => send({ cmd: "step_preset", dir: 1 }));
noFocus($("presetName")).addEventListener("click", () => privileged && editor.open("presets"));
noFocus($("presetsKey")).addEventListener("click", () => editor.open("presets"));
noFocus($("settingsKey")).addEventListener("click", () => editor.open("setup"));
noFocus($("fsKey")).addEventListener("click", () => send({ cmd: "fullscreen" }));
noFocus($("recKey")).addEventListener("click", () => send({ cmd: "toggle" }));

const MODE_LABEL: Record<string, string> = { mono: "Mono", stereo: "Stereo", multitrack: "Multitrack" };
const FMT_LABEL: Record<string, string> = { wav16: "16-bit WAV", wav24: "24-bit WAV", wav32f: "32-bit float WAV" };

// ─── status strip ─────────────────────────────────────────────────────────
interface Mod { el: HTMLElement; led: HTMLElement; title: HTMLElement; val: HTMLElement }
function mod(extra = ""): Mod {
  const l = led("off"), title = h("div", { class: "mod-title" }), val = h("div", { class: "mod-val" });
  const el = h("div", { class: "mod " + extra }, l, h("div", { class: "mod-text" }, title, val));
  return { el, led: l, title, val };
}
function setMod(m: Mod, color: string, title: string, val: string, cls = "", blink = false) {
  setLed(m.led, color, blink);
  setText(m.title, title);
  setText(m.val, val);
  m.val.className = "mod-val" + (cls ? " " + cls : "");
}
const master = mod("master"), input = mod(), queue = mod(), disk = mod(), remote = mod();
master.led.classList.add("led-lg");
const destMods: Mod[] = [];
const destWrap = h("div", { style: "display:contents" });
const spacer = h("div", { class: "mod spacer" });
$("status").append(master.el, input.el, destWrap, spacer, queue.el, disk.el, remote.el);
remote.el.style.cursor = "pointer";
remote.el.addEventListener("click", () => privileged && editor.open("setup"));
input.el.style.cursor = "pointer";
input.el.addEventListener("click", () => privileged && editor.open("presets"));

function render() {
  const s = state!;
  const p = s.presets.find((x) => x.id === s.active) || s.presets[0];
  const rec = s.rec;
  toggleClass(document.body, "recording", !!rec);

  // header
  setText($("presetName"), p ? p.name : "-");
  const nTracks = s.tracks.length;
  const enabledDests = p ? p.destinations.filter((d) => d.enabled).length : 0;
  setText($("presetSub"), p ? `${MODE_LABEL[p.mode]}${p.mode === "multitrack" ? ` · ${nTracks} tracks` : ""}  ·  ${FMT_LABEL[p.format]}  ·  ${enabledDests || 1} destination${enabledDests > 1 ? "s" : ""}` : "");
  ($("prevPreset") as HTMLButtonElement).disabled = !!rec;
  ($("nextPreset") as HTMLButtonElement).disabled = !!rec;
  toggleClass($("fsKey"), "on", s.settings.fullscreen);

  // input
  const a = s.audio;
  if (!a || a.state === "opening") setMod(input, "amber", "INPUT", "Opening device…", "", true);
  else if (a.state === "error") setMod(input, "red", "INPUT", a.message || "No input", "err");
  else {
    const rate = `${(a.sample_rate / 1000).toFixed(a.sample_rate % 1000 ? 1 : 0)} kHz`;
    const chs = a.selected.length === 1 ? `in ${a.selected[0] + 1}` : a.selected.length === 2 ? `in ${a.selected[0] + 1}-${a.selected[1] + 1}` : `${a.selected.length} of ${a.device_channels} in`;
    setMod(input, a.message ? "amber" : "green", `INPUT · ${rate} · ${chs}`, a.device, a.message ? "warn" : "");
    input.el.title = a.message || "";
  }

  // destinations
  while (destMods.length < s.dests.length) { const m = mod(); destMods.push(m); destWrap.appendChild(m.el); }
  while (destMods.length > s.dests.length) destWrap.removeChild(destMods.pop()!.el);
  s.dests.forEach((d, i) => {
    const m = destMods[i];
    const title = d.label.toUpperCase();
    if (d.state === "ready") setMod(m, "green", title, `Ready · ${fmtBytes(d.free_bytes)} free`);
    else if (d.state === "checking") setMod(m, "amber", title, "Checking…", "", true);
    else if (d.state === "mounting") setMod(m, "amber", title, "Connecting share…", "warn", true);
    else setMod(m, "red", title, d.message, "err");
    m.el.title = d.path;
  });

  // queue
  const failed = s.jobs.filter((j) => j.state === "failed").length;
  const busy = s.jobs.filter((j) => j.state === "busy" || j.state === "pending").length;
  if (failed) setMod(queue, "red", "DELIVERY", `${failed} take${failed > 1 ? "s" : ""} waiting to retry`, "err");
  else if (busy) setMod(queue, "amber", "DELIVERY", `Writing ${busy} take${busy > 1 ? "s" : ""}…`, "warn", true);
  else setMod(queue, "green", "DELIVERY", s.jobs.length ? "All takes delivered" : "Nothing queued");
  queue.el.style.cursor = failed ? "pointer" : "";
  queue.el.onclick = failed ? () => send({ cmd: "retry_jobs" }) : null;

  // disk (spool) with hours of recording left at the current track count
  const bps = (a && a.sample_rate ? a.sample_rate : 48000) * Math.max(1, nTracks) * 4;
  const hours = s.spool_free / bps / 3600;
  const hrs = hours >= 100 ? "100+ h" : hours >= 10 ? `${Math.round(hours)} h` : `${hours.toFixed(1)} h`;
  setMod(disk, hours > 3 ? "green" : hours > 0.75 ? "amber" : "red", "BUFFER DISK", `${fmtBytes(s.spool_free)} · ${hrs} left`, hours > 3 ? "" : hours > 0.75 ? "warn" : "err");

  // remote
  const others = s.remote.clients;
  if (!s.remote.enabled) setMod(remote, "off", "REMOTE", "Off");
  else setMod(remote, others > 0 ? "steel" : "off", "REMOTE", others > 0 ? `${others} device${others > 1 ? "s" : ""} connected` : (s.remote.urls[0] || "").replace("http://", ""));

  // master
  const destBad = s.dests.some((d) => d.state === "error");
  const destWait = s.dests.some((d) => d.state !== "ready");
  if (rec) setMod(master, "red", "SYSTEM", rec.stopping ? "SAVING" : "RECORDING", "", !rec.stopping);
  else if (!s.ready) setMod(master, "red", "SYSTEM", "NO INPUT", "err");
  else if (destBad || failed) setMod(master, "amber", "SYSTEM", "READY*", "warn");
  else if (destWait) setMod(master, "amber", "SYSTEM", "STARTING", "warn", true);
  else setMod(master, "green", "SYSTEM", "READY");
  master.el.title = destBad ? "Recording works. Takes for unreachable destinations wait in the buffer and are delivered when they come back." : "";

  // transport
  const key = $("recKey");
  toggleClass(key, "live", !!rec);
  toggleClass(key, "armed", !rec && s.ready);
  toggleClass(key, "blocked", !rec && !s.ready);
  setLed($("recLed"), rec ? "red" : s.ready ? "redoff" : "redoff", !!rec && !rec.stopping);
  $("recLed").classList.add("led-lg", "rec-led");
  setText($("recLabel"), rec ? (rec.stopping ? "SAVING" : "STOP") : "REC");
  setText($("takeName"), rec ? rec.take : s.jobs[0] ? `Last: ${s.jobs[0].take}` : "Ready when you are");

  const dests = s.dests.map((d) => d.label).join(" + ") || "Music folder";
  const msg = $("clockMsg");
  if (rec) msg.innerHTML = rec.stopping ? "Finishing take…" : `Recording <b>${rec.tracks.length}</b> track${rec.tracks.length > 1 ? "s" : ""} · ${privileged ? "<b>SPACE</b> to stop" : "tap <b>STOP</b> when done"}`;
  else if (!s.ready) msg.innerHTML = `<b>No input.</b> ${a && a.message ? escapeHtml(a.message) : "Check the audio interface."}`;
  else msg.innerHTML = `${privileged ? "Press <b>SPACE</b> to record" : "Tap <b>REC</b> to record"} · goes to ${escapeHtml(dests)}`;
  renderClock();

  // overlay hint when there's no input at all
  const ov = $("bayOverlay");
  clear(ov);
  if (!s.ready && !rec) ov.append(h("div", { class: "big" }, "WAITING FOR INPUT"), h("div", { class: "small" }, a && a.message ? a.message : "Connect your audio interface. Recording starts as soon as it shows up."));

  wave.setTracks(rec ? rec.tracks : s.tracks);
  renderTakes(s.jobs, s.now);
  setText($("queueInfo"), s.queue_open ? `${s.queue_open} in queue` : "");
  renderToast();
}

function escapeHtml(t: string) {
  return t.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c] as string));
}

let lastClock = "";
function renderClock() {
  const s = state;
  if (!s) return;
  let txt: string;
  if (s.rec && liveRecording) txt = fmtClock(liveFrames / (s.rec.sample_rate || 48000), true);
  else if (s.rec) txt = fmtClock((s.rec.frames || 0) / (s.rec.sample_rate || 48000), true);
  else txt = s.jobs[0] ? fmtClock(s.jobs[0].seconds, true) : "00:00:00.0";
  if (txt === lastClock) return;
  lastClock = txt;
  const [main, frac] = txt.split(".");
  const el = $("clock");
  el.innerHTML = `${main}<small>.${frac}</small>`;
}

// ─── takes list ───────────────────────────────────────────────────────────
function renderTakes(jobs: JobView[], now: number) {
  const list = $("takes");
  clear(list);
  if (!jobs.length) {
    list.appendChild(h("div", { class: "takes-empty" }, "No takes yet. The first one is always the best one."));
    return;
  }
  const rows = Math.max(1, Math.floor((list.clientHeight - 8) / 44));
  for (const j of jobs.slice(0, rows)) {
    const color = j.state === "done" ? "green" : j.state === "failed" ? "red" : "amber";
    const dests = j.tasks.map((t) => {
      const c = t.state === "done" ? "green" : t.state === "failed" ? "red" : t.state === "busy" ? "amber" : "off";
      const extra = t.state === "busy" ? h("span", { class: "bar" }, h("i", { style: `width:${Math.round(t.progress * 100)}%` }))
        : t.state === "failed" ? ` · retry ${Math.max(0, Math.ceil((t.next_try - now) / 1000))}s` : null;
      return h("span", { class: "tdest" + (t.state === "failed" ? " failed" : ""), title: t.out || t.message }, led(c as any, "led-sm" + (t.state === "busy" ? " pulse" : "")), t.label, extra);
    });
    list.appendChild(h("div", { class: "take", title: j.note || "" },
      led(color as any, j.state === "busy" ? "pulse" : j.state === "pending" ? "blink" : ""),
      h("div", { style: "min-width:0" }, h("div", { class: "take-title" }, j.take + (j.note ? "  ⚠" : "")), h("div", { class: "take-dests" }, dests)),
      h("div", { class: "take-len" }, fmtDur(j.seconds))));
  }
}

// ─── toast ────────────────────────────────────────────────────────────────
let toastId = 0, toastTimer = 0;
function renderToast() {
  const t = state && state.toast;
  if (!t || t.id === toastId) return;
  toastId = t.id;
  const el = $("toast");
  clear(el);
  el.append(led(t.level === "ok" ? "green" : "red"), h("span", null, t.msg));
  toggleClass(el, "hidden", false);
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => toggleClass(el, "hidden", true), t.level === "ok" ? 3000 : 7000);
}
$("toast").addEventListener("click", () => toggleClass($("toast"), "hidden", true));

// ─── keyboard: SPACE is the whole user manual ───────────────────────────────
function typing(e: KeyboardEvent) {
  const t = e.target as HTMLElement;
  return !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT" || t.isContentEditable);
}
window.addEventListener("keydown", (e) => {
  if (typing(e)) {
    if (e.key === "Escape") (e.target as HTMLElement).blur();
    return;
  }
  if (e.code === "Space" || e.key === " ") {
    e.preventDefault();
    if (!e.repeat) send({ cmd: "toggle" });
    return;
  }
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  if (e.key === "Escape") editor.close();
  else if ((e.key === "f" || e.key === "F") && privileged) send({ cmd: "fullscreen" });
  else if (e.key === "ArrowLeft" && !editor.isOpen()) send({ cmd: "step_preset", dir: -1 });
  else if (e.key === "ArrowRight" && !editor.isOpen()) send({ cmd: "step_preset", dir: 1 });
});
// keep buttons from stealing focus (SPACE would click them)
document.addEventListener("focusin", (e) => {
  const t = e.target as HTMLElement;
  if (t && t.tagName === "BUTTON") t.blur();
});
// no rubber-band scrolling / pinch on tablets
document.addEventListener("touchmove", (e) => { if (!(e.target as HTMLElement).closest(".form, .plist")) e.preventDefault(); }, { passive: false });
document.addEventListener("contextmenu", (e) => { if (!typing(e as any)) e.preventDefault(); });

// re-render takes when the panel size changes
window.addEventListener("resize", () => state && renderTakes(state.jobs, state.now));

connect();
if ((document as any).fonts && (document as any).fonts.ready) (document as any).fonts.ready.then(() => wave.invalidate());
// deep links: #presets / #setup open the sheet (handy for screenshots and kiosk setups)
const deep = location.hash.slice(1);
if (deep === "presets" || deep === "setup") {
  const t = setInterval(() => { if (state && privileged) { clearInterval(t); editor.open(deep as any); } }, 100);
}
