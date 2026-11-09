import { describe, it, expect, beforeAll, vi } from "vitest";
import { FakeWS, loadPage } from "./setup";
import { state, preset, job, dest } from "./fixtures";
import type { AppState } from "../src/types";

let ws: FakeWS;
const $ = (id: string) => document.getElementById(id) as HTMLElement;
const text = (id: string) => $(id).textContent;
const mods = () => [...document.querySelectorAll("#status .mod")].map((m) => ({
  title: m.querySelector(".mod-title")?.textContent, val: m.querySelector(".mod-val")?.textContent,
  led: m.querySelector(".led")?.className, cls: m.querySelector(".mod-val")?.className,
}));
const mod = (title: string) => mods().find((m) => (m.title || "").startsWith(title))!;
const modEl = (title: string) => [...document.querySelectorAll("#status .mod")].find((m) => (m.querySelector(".mod-title")?.textContent || "").startsWith(title)) as HTMLElement;
const push = (o: Partial<AppState> = {}) => ws.msg(state(o));
const key = (k: string, o: KeyboardEventInit = {}, target: EventTarget = document.body) =>
  target.dispatchEvent(new KeyboardEvent("keydown", { key: k, code: k === " " ? "Space" : k, bubbles: true, cancelable: true, ...o }));

beforeAll(async () => {
  loadPage();
  await import("../src/main");
  ws = FakeWS.last();
  ws.open();
  ws.msg({ t: "hello", privileged: true });
});

describe("connection", () => {
  it("connects to /ws on the serving host", () => {
    expect(ws.url).toBe("ws://127.0.0.1:7878/ws");
    expect($("offline").classList.contains("hidden")).toBe(true);
  });

  it("shows the offline card and reconnects with backoff", () => {
    vi.useFakeTimers();
    const n = FakeWS.instances.length;
    ws.close();
    expect($("offline").classList.contains("hidden")).toBe(false);
    expect(text("offlineMsg")).toBe("Reconnecting…");
    vi.advanceTimersByTime(400);
    expect(FakeWS.instances.length).toBe(n + 1);
    for (let i = 0; i < 4; i++) { FakeWS.last().close(); vi.advanceTimersByTime(4000); }
    expect(text("offlineMsg")).toBe("Is the recorder app running? Retrying…");
    ws = FakeWS.last();
    ws.open();
    expect($("offline").classList.contains("hidden")).toBe(true);
    ws.msg({ t: "hello", privileged: true });
    vi.useRealTimers();
  });

  it("drops commands while disconnected", () => {
    ws.readyState = 0;
    key(" ");
    expect(ws.sent).toEqual([]);
    ws.readyState = 1;
  });

  it("marks remote clients", () => {
    ws.msg({ t: "hello", privileged: false });
    expect(document.body.classList.contains("remote")).toBe(true);
    ws.msg({ t: "hello", privileged: true });
    expect(document.body.classList.contains("remote")).toBe(false);
  });

  it("ignores unknown messages and stray picks", () => {
    ws.msg({ t: "what" });
    ws.msg({ t: "picked", req: "nobody", path: "/x" });
  });
});

describe("header", () => {
  it("shows the preset", () => {
    push();
    expect(text("presetName")).toBe("Band Practice");
    expect(text("presetSub")).toBe("Stereo  ·  24-bit WAV  ·  1 destination");
    push({ presets: [preset({ mode: "multitrack", format: "wav32f", destinations: [dest(), dest({ id: "d2" })] })], tracks: ["a", "b", "c"] });
    expect(text("presetSub")).toBe("Multitrack · 3 tracks  ·  32-bit float WAV  ·  2 destinations");
    push({ presets: [preset({ destinations: [dest({ enabled: false })] })] });
    expect(text("presetSub")).toContain("1 destination");
    push({ active: "gone" });
    expect(text("presetName")).toBe("Band Practice");
    push({ presets: [], active: "" });
    expect(text("presetName")).toBe("-");
    expect(text("presetSub")).toBe("");
  });

  it("buttons send commands", () => {
    push();
    ws.sent = [];
    for (const id of ["prevPreset", "nextPreset", "fsKey", "recKey"]) $(id).click();
    expect(ws.sent).toEqual([{ cmd: "step_preset", dir: -1 }, { cmd: "step_preset", dir: 1 }, { cmd: "fullscreen" }, { cmd: "toggle" }]);
    expect($("fsKey").classList.contains("on")).toBe(true);
  });

  it("preset arrows lock while recording", () => {
    push({ rec: { take: "t", started_at: 0, started: true, stopping: false, frames: 0, sample_rate: 48000, tracks: ["L", "R"] } });
    expect(($("prevPreset") as HTMLButtonElement).disabled).toBe(true);
    push();
    expect(($("nextPreset") as HTMLButtonElement).disabled).toBe(false);
  });
});

describe("status strip", () => {
  it("input module", () => {
    push({ audio: null });
    expect(mod("INPUT").val).toBe("Opening device…");
    push({ audio: { ...state().audio!, state: "error", message: "'Scarlett' not connected" } });
    expect(mod("INPUT")).toMatchObject({ val: "'Scarlett' not connected", cls: "mod-val err" });
    push({ audio: { ...state().audio!, state: "error", message: "" } });
    expect(mod("INPUT").val).toBe("No input");
    push();
    expect(mod("INPUT").title).toBe("INPUT · 48 kHz · in 1-2");
    push({ audio: { ...state().audio!, sample_rate: 44100, selected: [3], message: "Device runs at 44100 Hz" } });
    expect(mod("INPUT")).toMatchObject({ title: "INPUT · 44.1 kHz · in 4", cls: "mod-val warn" });
    push({ audio: { ...state().audio!, selected: [0, 1, 2, 3], device_channels: 8 } });
    expect(mod("INPUT").title).toBe("INPUT · 48 kHz · 4 of 8 in");
  });

  it("destination modules follow the list", () => {
    const ds = (st: any[]) => st.map((s, i) => ({ id: "d" + i, label: "Nas" + i, path: "/p", state: s, message: "Share not responding", free_bytes: 1e12 }));
    push({ dests: ds(["ready", "checking", "mounting", "error"]) });
    expect(mod("NAS0").val).toBe("Ready · 931 GB free");
    expect(mod("NAS1").val).toBe("Checking…");
    expect(mod("NAS2").val).toBe("Connecting share…");
    expect(mod("NAS3")).toMatchObject({ val: "Share not responding", cls: "mod-val err" });
    expect(mods().find((m) => m.title === "SYSTEM")!.val).toBe("READY*");
    push({ dests: ds(["checking"]) });
    expect(mods().filter((m) => (m.title || "").startsWith("NAS")).length).toBe(1);
    expect(mods().find((m) => m.title === "SYSTEM")!.val).toBe("STARTING");
  });

  it("delivery module, click to retry", () => {
    push({ jobs: [job({ state: "failed" }), job({ state: "failed" })] });
    expect(mod("DELIVERY").val).toBe("2 takes waiting to retry");
    expect(mods().find((m) => m.title === "SYSTEM")!.val).toBe("READY*");
    ws.sent = [];
    modEl("DELIVERY").click();
    expect(ws.sent).toEqual([{ cmd: "retry_jobs" }]);
    push({ jobs: [job({ state: "failed" })] });
    expect(mod("DELIVERY").val).toBe("1 take waiting to retry");
    push({ jobs: [job({ state: "busy" }), job({ state: "pending" })] });
    expect(mod("DELIVERY").val).toBe("Writing 2 takes…");
    push({ jobs: [job({ state: "busy" })] });
    expect(mod("DELIVERY").val).toBe("Writing 1 take…");
    push({ jobs: [job()] });
    expect(mod("DELIVERY").val).toBe("All takes delivered");
    push();
    expect(mod("DELIVERY").val).toBe("Nothing queued");
  });

  it("buffer disk shows hours left", () => {
    push({ spool_free: 48000 * 2 * 4 * 3600 * 200 });
    expect(mod("BUFFER")).toMatchObject({ val: "257 GB · 100+ h left", cls: "mod-val" });
    push({ spool_free: 48000 * 2 * 4 * 3600 * 20 });
    expect(mod("BUFFER").val).toContain("· 20 h left");
    push({ spool_free: 48000 * 2 * 4 * 3600 * 2 });
    expect(mod("BUFFER")).toMatchObject({ cls: "mod-val warn" });
    expect(mod("BUFFER").val).toContain("2.0 h left");
    push({ spool_free: 48000 * 2 * 4 * 600, audio: null, tracks: [] });
    expect(mod("BUFFER").cls).toBe("mod-val err");
  });

  it("remote module", () => {
    push({ remote: { ...state().remote, enabled: false } });
    expect(mod("REMOTE").val).toBe("Off");
    push();
    expect(mod("REMOTE").val).toBe("192.168.1.5:7878");
    push({ remote: { ...state().remote, clients: 1 } });
    expect(mod("REMOTE").val).toBe("1 device connected");
    push({ remote: { ...state().remote, clients: 3 } });
    expect(mod("REMOTE").val).toBe("3 devices connected");
    push({ remote: { ...state().remote, urls: [] } });
    expect(mod("REMOTE").val).toBe("");
  });

  it("master module and transport", () => {
    push({ ready: false, audio: { ...state().audio!, state: "error", message: "<img src=x onerror=alert(1)>" } });
    expect(mods().find((m) => m.title === "SYSTEM")!.val).toBe("NO INPUT");
    expect($("recKey").classList.contains("blocked")).toBe(true);
    expect($("clockMsg").querySelector("img")).toBeNull();
    expect(text("clockMsg")).toBe("No input. <img src=x onerror=alert(1)>");
    expect(text("bayOverlay")).toContain("WAITING FOR INPUT");
    push({ ready: false, audio: null });
    expect(text("clockMsg")).toBe("No input. Check the audio interface.");
    expect(text("bayOverlay")).toContain("Connect your audio interface");
    push({ dests: [{ ...state().dests[0], label: "<b>NAS</b>" }] });
    expect(mods().find((m) => m.title === "SYSTEM")!.val).toBe("READY");
    expect($("clockMsg").innerHTML).toBe("Press <b>SPACE</b> to record · goes to &lt;b&gt;NAS&lt;/b&gt;");
    expect($("recKey").classList.contains("armed")).toBe(true);
    expect(text("takeName")).toBe("Ready when you are");
    push({ dests: [] });
    expect(text("clockMsg")).toContain("goes to Music folder");
    expect(text("bayOverlay")).toBe("");
  });

  it("recording and saving", () => {
    const rec = { take: "Take 7", started_at: 0, started: true, stopping: false, frames: 96000, sample_rate: 48000, tracks: ["L", "R"] };
    push({ rec });
    expect(document.body.classList.contains("recording")).toBe(true);
    expect(mods().find((m) => m.title === "SYSTEM")!.val).toBe("RECORDING");
    expect(text("recLabel")).toBe("STOP");
    expect(text("takeName")).toBe("Take 7");
    expect(text("clockMsg")).toBe("Recording 2 tracks · SPACE to stop");
    expect(text("clock")).toBe("00:00:02.0");
    expect($("recLed").className).toContain("blink");
    push({ rec: { ...rec, stopping: true, tracks: ["M"], frames: 0, sample_rate: 0 } });
    expect(text("recLabel")).toBe("SAVING");
    expect(text("clockMsg")).toBe("Finishing take…");
    expect(text("clock")).toBe("00:00:00.0");
    ws.msg({ t: "hello", privileged: false });
    push({ rec: { ...rec, tracks: ["M"] } });
    expect(text("clockMsg")).toBe("Recording 1 track · tap STOP when done");
    push();
    expect(text("clockMsg")).toContain("Tap REC to record");
    ws.msg({ t: "hello", privileged: true });
  });

  it("live clock follows audio frames", () => {
    const rec = { take: "T", started_at: 0, started: true, stopping: false, frames: 0, sample_rate: 48000, tracks: ["L", "R"] };
    push({ rec });
    ws.msg({ t: "live", m: [500, 1000], b: [[1, 2], [3, 4]], f: 48000 * 61.5, r: true });
    expect(text("clock")).toBe("00:01:01.5");
    ws.msg({ t: "live", m: [500, 1000], b: [[1, 2], [3, 4]], f: 48000 * 61.5, r: true });
    push({ rec: { ...rec, sample_rate: 0 } });
    ws.msg({ t: "live", m: [], b: [], f: 48000, r: true });
    expect(text("clock")).toBe("00:00:01.0");
    push({ audio: null });
    ws.msg({ t: "live", m: [], b: [], f: 0, r: false });
    ws.msg({ t: "hist", rolling: [[1, 2]], take: [[]], recording: false, bps: 100 });
    push({ jobs: [job({ seconds: 3600 })] });
    expect(text("clock")).toBe("01:00:00.0");
    expect(text("takeName")).toBe("Last: Take 1");
  });
});

describe("takes list", () => {
  it("renders jobs with per-destination state", () => {
    Object.defineProperty($("takes"), "clientHeight", { value: 500, configurable: true });
    const tasks = [
      { label: "Local", state: "done", progress: 1, message: "", out: "/m/t.wav", next_try: 0 },
      { label: "NAS", state: "failed", progress: 0, message: "Destination not reachable", out: "", next_try: 1_012_400 },
      { label: "USB", state: "busy", progress: 0.42, message: "Writing", out: "", next_try: 0 },
      { label: "Old", state: "failed", progress: 0, message: "x", out: "", next_try: 0 },
      { label: "Q", state: "pending", progress: 0, message: "Queued", out: "", next_try: 0 },
    ] as any;
    push({ jobs: [job({ tasks, state: "failed", note: "Recording was cut short" }), job({ state: "busy" }), job({ state: "pending" })], queue_open: 3 });
    const rows = document.querySelectorAll("#takes .take");
    expect(rows.length).toBe(3);
    expect(rows[0].querySelector(".take-title")!.textContent).toBe("Take 1  ⚠");
    expect((rows[0] as HTMLElement).title).toBe("Recording was cut short");
    expect(rows[0].textContent).toContain("NAS · retry 13s");
    expect(rows[0].textContent).toContain("Old · retry 0s");
    expect((rows[0].querySelector(".bar i") as HTMLElement).style.width).toBe("42%");
    expect(rows[0].querySelector(".take-len")!.textContent).toBe("1:15");
    expect(text("queueInfo")).toBe("3 in queue");
  });

  it("limits rows to the space there is, empty state", () => {
    Object.defineProperty($("takes"), "clientHeight", { value: 0, configurable: true });
    push({ jobs: [job(), job(), job()] });
    expect(document.querySelectorAll("#takes .take").length).toBe(1);
    window.dispatchEvent(new Event("resize"));
    push();
    expect(text("takes")).toContain("No takes yet");
    expect(text("queueInfo")).toBe("");
  });
});

describe("toast", () => {
  it("shows once per id and hides itself", () => {
    vi.useFakeTimers();
    push({ toast: { id: 1, level: "ok", msg: "Preset saved" } });
    expect($("toast").classList.contains("hidden")).toBe(false);
    expect(text("toast")).toBe("Preset saved");
    vi.advanceTimersByTime(3100);
    expect($("toast").classList.contains("hidden")).toBe(true);
    push({ toast: { id: 1, level: "ok", msg: "Preset saved" } });
    expect($("toast").classList.contains("hidden")).toBe(true);
    push({ toast: { id: 2, level: "error", msg: "Disk full" } });
    vi.advanceTimersByTime(5000);
    expect($("toast").classList.contains("hidden")).toBe(false);
    $("toast").click();
    expect($("toast").classList.contains("hidden")).toBe(true);
    vi.useRealTimers();
  });
});

describe("keyboard", () => {
  it("SPACE toggles, held key does not repeat", () => {
    push();
    ws.sent = [];
    expect(key(" ")).toBe(false);
    key(" ", { repeat: true });
    expect(ws.sent).toEqual([{ cmd: "toggle" }]);
  });

  it("typing in a field never records", () => {
    const inp = document.createElement("input");
    document.body.appendChild(inp);
    inp.focus();
    ws.sent = [];
    key(" ", {}, inp);
    key("Enter", {}, inp);
    key("Escape", {}, inp);
    expect(document.activeElement).not.toBe(inp);
    for (const tag of ["textarea", "select"]) key(" ", {}, document.body.appendChild(document.createElement(tag)));
    const ce = document.createElement("div");
    Object.defineProperty(ce, "isContentEditable", { value: true });
    key(" ", {}, document.body.appendChild(ce));
    expect(ws.sent).toEqual([]);
  });

  it("F, arrows, modifiers", () => {
    ws.sent = [];
    key("f");
    key("F");
    key("ArrowLeft");
    key("ArrowRight");
    key("ArrowRight", { metaKey: true });
    key("x");
    expect(ws.sent).toEqual([{ cmd: "fullscreen" }, { cmd: "fullscreen" }, { cmd: "step_preset", dir: -1 }, { cmd: "step_preset", dir: 1 }]);
    ws.msg({ t: "hello", privileged: false });
    ws.sent = [];
    key("f");
    expect(ws.sent).toEqual([]);
    ws.msg({ t: "hello", privileged: true });
  });

  it("buttons can't keep focus, page doesn't scroll or show menus", () => {
    const b = document.createElement("button");
    document.body.appendChild(b);
    b.focus();
    expect(document.activeElement).not.toBe(b);
    const tm = new Event("touchmove", { cancelable: true, bubbles: true });
    $("status").dispatchEvent(tm);
    expect(tm.defaultPrevented).toBe(true);
    const form = document.createElement("div");
    form.className = "form";
    document.body.appendChild(form);
    const tm2 = new Event("touchmove", { cancelable: true, bubbles: true });
    form.dispatchEvent(tm2);
    expect(tm2.defaultPrevented).toBe(false);
    const cm = new MouseEvent("contextmenu", { cancelable: true, bubbles: true });
    $("status").dispatchEvent(cm);
    expect(cm.defaultPrevented).toBe(true);
    const inp = document.body.appendChild(document.createElement("input"));
    const cm2 = new MouseEvent("contextmenu", { cancelable: true, bubbles: true });
    inp.dispatchEvent(cm2);
    expect(cm2.defaultPrevented).toBe(false);
    const other = new Event("focusin", { bubbles: true });
    inp.dispatchEvent(other);
  });
});

describe("editor wiring", () => {
  it("opens from header keys and modules, ESC closes, arrows stay in the sheet", () => {
    push();
    $("presetsKey").click();
    expect($("sheet").classList.contains("hidden")).toBe(false);
    ws.sent = [];
    key("ArrowLeft");
    expect(ws.sent).toEqual([]);
    key("Escape");
    expect($("sheet").classList.contains("hidden")).toBe(true);
    $("settingsKey").click();
    expect(text("sheet")).toContain("REMOTE CONTROL");
    key("Escape");
    $("presetName").click();
    expect($("sheet").classList.contains("hidden")).toBe(false);
    key("Escape");
    modEl("INPUT").click();
    expect($("sheet").classList.contains("hidden")).toBe(false);
    key("Escape");
    modEl("REMOTE").click();
    expect(text("sheet")).toContain("REMOTE CONTROL");
    key("Escape");
    ws.msg({ t: "hello", privileged: false });
    $("presetName").click();
    modEl("INPUT").click();
    expect($("sheet").classList.contains("hidden")).toBe(true);
    ws.msg({ t: "hello", privileged: true });
  });

  it("folder picker round-trips through the server", async () => {
    push();
    $("presetsKey").click();
    ws.sent = [];
    const browse = [...document.querySelectorAll("#sheet .key")].find((b) => b.textContent === "BROWSE") as HTMLElement;
    browse.click();
    const req = ws.sent.find((c) => c.cmd === "pick_folder");
    expect(req.start).toBe("/music");
    ws.msg({ t: "picked", req: req.req, path: "/Volumes/NAS" });
    await Promise.resolve();
    await Promise.resolve();
    expect((document.querySelector("#sheet .dest-path input") as HTMLInputElement).value).toBe("/Volumes/NAS");
    // live meters reach the editor while it's open
    ws.msg({ t: "live", m: [1000, 0], b: [], f: 0, r: false });
    expect((document.querySelector("#sheet .ch .mtr i") as HTMLElement).style.width).toBe("100%");
  });

  it("chat lookup round-trips, and times out without an answer", async () => {
    push({ presets: [preset({ destinations: [dest({ kind: "telegram", token: "••••abcd" })] })] });
    if ($("sheet").classList.contains("hidden")) $("presetsKey").click();
    const revert = () => [...document.querySelectorAll<HTMLElement>("#sheet .key")].find((b) => b.textContent === "REVERT")!.click();
    revert();
    ws.sent = [];
    const find = [...document.querySelectorAll<HTMLElement>("#sheet .key")].find((b) => b.textContent === "FIND CHATS")!;
    find.click();
    const req = ws.sent.find((c) => c.cmd === "find_chats");
    expect(req.dest.token).toBe("••••abcd");
    ws.msg({ t: "chats", req: "stray", ok: true, msg: "", bot: "", chats: [] });
    ws.msg({ t: "chats", req: req.req, ok: true, msg: "", bot: "@b", chats: [{ id: "-1", name: "Band", kind: "group" }] });
    await vi.waitFor(() => expect(document.querySelector("#sheet .chat b")!.textContent).toBe("Band"));
    vi.useFakeTimers();
    find.click();
    vi.advanceTimersByTime(30001);
    vi.useRealTimers();
    await vi.waitFor(() => expect(document.querySelector("#sheet .chat-msg")!.textContent).toBe("No answer from the recorder"));
    push();
    revert();
  });

  it("destination test round-trips, and times out without an answer", async () => {
    push();
    $("presetsKey").click();
    ws.sent = [];
    const test = [...document.querySelectorAll<HTMLElement>("#sheet .key")].find((b) => b.textContent === "TEST")!;
    test.click();
    const req = ws.sent.find((c) => c.cmd === "test_dest");
    expect(req.dest.id).toBe("d1");
    ws.msg({ t: "dest_test", req: req.req, ok: true, msg: "", path: "/Volumes/NAS", free: 1e9 });
    ws.msg({ t: "dest_test", req: "stray", ok: true, msg: "", path: "", free: 0 });
    await vi.waitFor(() => expect(document.querySelector("#sheet .dpill-text")!.textContent).toBe("Works · 954 MB free · /Volumes/NAS"));
    vi.useFakeTimers();
    test.click();
    vi.advanceTimersByTime(30001);
    vi.useRealTimers();
    await vi.waitFor(() => expect(document.querySelector("#sheet .dpill-text")!.textContent).toBe("No answer from the recorder"));
  });
});
