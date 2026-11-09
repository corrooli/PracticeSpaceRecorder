import { describe, it, expect, beforeEach, vi } from "vitest";
import { Editor, expandTemplate } from "../src/editor";
import { state, preset, dest } from "./fixtures";
import type { AppState, LiveMsg } from "../src/types";

let ed: Editor;
let sent: any[];
let live: ((m: LiveMsg) => void)[];
let meters: number[];
let privileged: boolean;
let pick: (start: string) => Promise<string | null>;
let tester: (d: any) => Promise<any>;
let finder: (d: any) => Promise<any>;
const sheet = () => document.getElementById("sheet")!;
const keys = () => [...sheet().querySelectorAll<HTMLElement>(".key")];
const keyBtn = (label: string) => keys().find((k) => k.textContent === label)!;
const press = (label: string) => keyBtn(label).click();
const hint = () => sheet().querySelector(".foot-hint")!.textContent;
const draft = () => (ed as any).draft;
const lastSaved = () => sent.filter((c) => c.cmd === "save_preset").pop();
const segBtn = (label: string) => [...sheet().querySelectorAll<HTMLElement>(".seg button")].find((b) => b.textContent === label)!;
const ch = (n: number) => [...sheet().querySelectorAll<HTMLElement>(".ch")][n - 1];
const inputs = () => [...sheet().querySelectorAll<HTMLInputElement>("input")];
const type = (el: HTMLInputElement, v: string) => { el.value = v; el.dispatchEvent(new Event("input")); };
const selects = () => [...sheet().querySelectorAll<HTMLSelectElement>("select")];

function boot(s: AppState = state()) {
  document.body.innerHTML = '<div id="sheet" class="sheet hidden"></div>';
  sent = []; live = []; meters = []; privileged = true;
  pick = async () => "/Picked";
  tester = async () => ({ ok: true, msg: "", path: "/Volumes/Oliver/Test", free: 2e12 });
  finder = async () => ({ ok: true, msg: "", bot: "@band_bot", chats: [] });
  ed = new Editor();
  ed.init({
    send: (c) => sent.push(JSON.parse(JSON.stringify(c))),
    pickFolder: (s) => pick(s),
    testDest: (d) => tester(d),
    findChats: (d) => finder(d),
    onLive: (f) => { live.push(f); return () => { live = live.filter((x) => x !== f); }; },
    getMeters: () => meters,
    isPrivileged: () => privileged,
  });
  ed.update(s);
}

beforeEach(() => boot());

describe("expandTemplate", () => {
  const d = new Date(2026, 2, 4, 5, 6, 7);
  it("matches the Rust side", () => {
    expect(expandTemplate("{year}/{month}/{day}/{date} {time} {preset} {take}", "Band/X", 7, d)).toBe("2026 / 03 / 04 / 2026-03-04 05-06-07 Band-X 07");
    expect(expandTemplate("  ", "P", 1, d)).toBe("2026-03-04 05-06-07 P");
    expect(expandTemplate("a\\\\b//c: d", "P", 1, d)).toBe("a / b / c- d");
    expect(expandTemplate("{take}", "P", 1)).toBe("01");
  });
});

describe("open / close", () => {
  it("only on the recording computer, only with state", () => {
    privileged = false;
    ed.open("presets");
    expect(ed.isOpen()).toBe(false);
    privileged = true;
    const fresh = new Editor();
    fresh.init({ send() {}, pickFolder: async () => null, testDest: async () => ({ ok: true, msg: "", path: "", free: 0 }), onLive: () => () => {}, getMeters: () => [], isPrivileged: () => true });
    fresh.open("presets");
    expect(fresh.isOpen()).toBe(false);
    ed.close(); // closed already: nothing
    ed.open("presets");
    expect(ed.isOpen()).toBe(true);
    expect(live.length).toBe(1);
    ed.open("presets");
    expect(live.length).toBe(1, );
    ed.close();
    expect(ed.isOpen()).toBe(false);
    expect(live.length).toBe(0);
  });

  it("clicking the backdrop closes, unsaved changes block closing", () => {
    ed.open("presets");
    sheet().querySelector(".sheet-card")!.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(ed.isOpen()).toBe(true);
    sheet().dispatchEvent(new MouseEvent("mousedown"));
    expect(ed.isOpen()).toBe(false);
    ed.open("presets");
    type(inputs()[0], "Renamed");
    expect(hint()).toBe("Unsaved changes");
    expect(sheet().querySelector(".plist-item.sel")!.textContent).toBe("Renamed  •");
    press("✕");
    expect(ed.isOpen()).toBe(true);
    expect(hint()).toBe("Unsaved changes. SAVE them, or REVERT first.");
    // reopening keeps the draft
    ed.open("setup");
    ed.open("presets");
    expect(inputs()[0].value).toBe("Renamed");
    press("REVERT");
    expect(inputs()[0].value).toBe("Band Practice");
    press("✕");
    expect(ed.isOpen()).toBe(false);
  });

  it("tabs switch", () => {
    ed.open("presets");
    press("SETUP");
    expect(sheet().querySelector(".sheet-title")!.textContent).toContain("SETUP");
    press("PRESETS");
    expect(sheet().querySelector(".sheet-title")!.textContent).toContain("PRESETS");
    expect(sheet().textContent).toContain("v1.0.0");
  });
});

describe("presets", () => {
  it("saves the draft with trimmed track names", () => {
    boot(state({ presets: [preset({ track_names: ["a", "b", "c"] })] }));
    ed.open("presets");
    press("SAVE");
    expect(lastSaved()).toMatchObject({ activate: false, preset: { id: "p1", track_names: ["a", "b"] } });
    expect(hint()).toBe("Saved.");
    press("SAVE & USE");
    expect(lastSaved().activate).toBe(true);
    expect(hint()).toBe("Saved and active.");
    boot(state({ presets: [preset({ track_names: [] })] }));
    ed.open("presets");
    press("SAVE");
    expect(lastSaved().preset.track_names).toEqual(["", ""]);
  });

  it("multitrack needs a channel", () => {
    ed.open("presets");
    segBtn("MULTITRACK").click();
    press("NONE");
    press("SAVE");
    expect(lastSaved()).toBeUndefined();
    expect(hint()).toBe("Pick at least one channel.");
  });

  it("switching presets in the list", () => {
    boot(state({ presets: [preset(), preset({ id: "p2", name: "Gig" })] }));
    ed.open("presets");
    const items = () => [...sheet().querySelectorAll<HTMLElement>(".plist-item")];
    expect(items()[0].querySelector(".led")!.className).toContain("green");
    items()[0].click(); // already selected
    items()[1].click();
    expect(inputs()[0].value).toBe("Gig");
    type(inputs()[0], "Gig 2");
    items()[0].click();
    expect(inputs()[0].value).toBe("Gig 2");
    expect(hint()).toContain("Unsaved changes. SAVE");
  });

  it("new preset and copy start from the active one", () => {
    ed.open("presets");
    press("+ NEW");
    expect(draft().name).toBe("New Preset");
    expect(draft().id).not.toBe("p1");
    expect(draft().destinations[0].id).not.toBe("d1");
    expect(sheet().querySelectorAll(".plist-item").length).toBe(2);
    press("+ NEW");
    expect(hint()).toContain("Unsaved changes. SAVE");
    press("COPY");
    expect(hint()).toContain("Unsaved changes. SAVE");
    // unsaved new preset: DELETE just drops the draft
    press("DELETE");
    expect(draft().id).toBe("p1");
    press("COPY");
    expect(draft().name).toBe("Band Practice copy");
  });

  it("delete asks first, and keeps at least one", () => {
    vi.useFakeTimers();
    ed.open("presets");
    press("DELETE");
    expect(hint()).toBe("You need at least one preset. It's the law.");
    boot(state({ presets: [preset(), preset({ id: "p2", name: "Gig" })] }));
    ed.open("presets");
    press("DELETE");
    expect(keyBtn("SURE?")).toBeTruthy();
    vi.advanceTimersByTime(2600);
    expect(keyBtn("DELETE")).toBeTruthy();
    press("DELETE");
    press("SURE?");
    expect(sent).toContainEqual({ cmd: "delete_preset", id: "p1" });
    expect(draft().id).toBe("p2");
    // the SURE? timer firing after a rebuild is harmless
    vi.advanceTimersByTime(3000);
    vi.useRealTimers();
  });

  it("follows the server: list refresh, preset deleted elsewhere", () => {
    ed.open("presets");
    ed.update(state({ presets: [preset({ name: "Renamed elsewhere" }), preset({ id: "p2", name: "Two" })] }));
    expect(sheet().querySelectorAll(".plist-item").length).toBe(2);
    boot(state({ presets: [preset(), preset({ id: "p2", name: "Two" })] }));
    ed.open("presets");
    [...sheet().querySelectorAll<HTMLElement>(".plist-item")][1].click();
    ed.update(state({ presets: [preset()] }));
    expect(draft().id).toBe("p1");
  });
});

describe("devices, rates, modes and channels", () => {
  const four = state({ devices: [{ name: "Scarlett 2i2", channels: 2, rates: [44100, 48000], is_default: true }, { name: "Octa", channels: 4, rates: [], is_default: false }] });

  it("device list, missing device, rates", () => {
    boot(state({ presets: [preset({ device: "Gone", sample_rate: 22050 })] }));
    ed.open("presets");
    const [dev, rate] = selects();
    expect([...dev.options].map((o) => o.textContent)).toEqual(["System default (Scarlett 2i2)", "Scarlett 2i2  ·  2 in", "Gone  (not connected)"]);
    expect([...rate.options].map((o) => o.value)).toEqual(["44100", "48000", "88200", "96000", "22050"]);
    dev.value = "Scarlett 2i2";
    dev.dispatchEvent(new Event("change"));
    expect(draft().device).toBe("Scarlett 2i2");
    expect([...selects()[1].options].map((o) => o.value)).toEqual(["44100", "48000", "22050"]);
    selects()[1].value = "44100";
    selects()[1].dispatchEvent(new Event("change"));
    expect(draft().sample_rate).toBe(44100);
    boot(state({ devices: [] }));
    ed.open("presets");
    expect(selects()[0].options[0].textContent).toBe("System default");
  });

  it("mode switches pick sensible channels", () => {
    boot(four);
    ed.open("presets");
    (ed as any).draft.device = "Octa";
    (ed as any).renderForm();
    segBtn("MONO").click();
    expect(draft().channels).toEqual([1]);
    segBtn("STEREO").click();
    expect(draft().channels).toEqual([1, 2]);
    segBtn("MULTITRACK").click();
    expect(draft().channels).toEqual([1, 2, 3, 4]);
    // stereo pair at the last input goes one down
    draft().channels = [4];
    segBtn("STEREO").click();
    expect(draft().channels).toEqual([4, 3]);
    draft().channels = [];
    segBtn("MONO").click();
    expect(draft().channels).toEqual([1]);
    draft().channels = [];
    segBtn("STEREO").click();
    expect(draft().channels).toEqual([1, 2]);
  });

  it("channel buttons per mode", () => {
    boot(four);
    ed.open("presets");
    draft().device = "Octa";
    (ed as any).renderForm();
    ch(3).click();
    expect(draft().channels).toEqual([3, 4]);
    ch(4).click();
    expect(draft().channels).toEqual([3, 4]);
    ch(1).dispatchEvent(new MouseEvent("click", { altKey: true }));
    expect(draft().channels).toEqual([3, 1]);
    expect(ch(3).querySelector("small")!.textContent).toBe("LEFT");
    expect(ch(1).querySelector("small")!.textContent).toBe("RIGHT");
    draft().channels = [];
    ch(2).dispatchEvent(new MouseEvent("click", { altKey: true }));
    expect(draft().channels).toEqual([1, 2]);
    segBtn("MONO").click();
    ch(4).click();
    expect(draft().channels).toEqual([4]);
    expect(ch(4).querySelector("small")!.textContent).toBe("MONO");
    segBtn("MULTITRACK").click();
    ch(2).click();
    expect(draft().channels).toEqual([1, 3, 4]);
    ch(2).click();
    expect(draft().channels).toEqual([1, 2, 3, 4]);
    expect(ch(3).querySelector("small")!.textContent).toBe("TRK 3");
    press("NONE");
    expect(sheet().textContent).not.toContain("TRACK NAMES");
    press("ALL");
    expect(draft().channels).toEqual([1, 2, 3, 4]);
  });

  it("track names follow their channel", () => {
    boot(state({ presets: [preset({ mode: "multitrack", channels: [1, 2], track_names: ["Kick", "Snare"] })] }));
    ed.open("presets");
    const nameInputs = () => [...sheet().querySelectorAll<HTMLInputElement>(".names input")];
    expect(nameInputs().map((i) => i.value)).toEqual(["Kick", "Snare"]);
    ch(1).click(); // remove 1
    expect(draft().track_names).toEqual(["Snare"]);
    type(nameInputs()[0], "Snare top");
    expect(draft().track_names).toEqual(["Snare top"]);
    ch(1).click();
    expect(draft().track_names).toEqual(["", "Snare top"]);
    draft().track_names = [];
    type(nameInputs()[1], "x");
    expect(draft().track_names).toEqual(["", "x"]);
    expect(nameInputs()[0].placeholder).toBe("In 1");
    segBtn("STEREO").click();
    expect(nameInputs().map((i) => i.placeholder)).toEqual(["Left", "Right"]);
    segBtn("MONO").click();
    expect(nameInputs()[0].placeholder).toBe("Mono");
  });

  it("warns about channels the device doesn't have", () => {
    boot(state({ presets: [preset({ device: "Scarlett 2i2", mode: "multitrack", channels: [1, 2, 8] })] }));
    ed.open("presets");
    expect(sheet().textContent).toContain("Scarlett 2i2 has 2 inputs. Channel 8 will fall back to the last one.");
    boot(state({ devices: [{ name: "Mic", channels: 1, rates: [48000], is_default: true }], presets: [preset({ device: "Mic", mode: "mono", channels: [3] })] }));
    ed.open("presets");
    expect(sheet().textContent).toContain("Mic has 1 input. Channel 3");
    // unknown device: grid as wide as the preset needs
    boot(state({ devices: [], presets: [preset({ device: "Gone", mode: "multitrack", channels: [1, 6] })] }));
    ed.open("presets");
    expect(sheet().querySelectorAll(".ch").length).toBe(6);
    expect(sheet().textContent).toContain("Gone");
  });

  it("re-renders when devices change, but not while typing", () => {
    ed.open("presets");
    ed.update(state({ devices: [{ name: "New", channels: 8, rates: [48000], is_default: true }] }));
    expect(selects()[0].textContent).toContain("New");
    const nameIn = inputs()[0];
    nameIn.focus();
    ed.update(state({ devices: [] }));
    expect(selects()[0].textContent).toContain("New");
  });

  it("channel meters", () => {
    ed.open("presets");
    meters = [1, 0.001];
    live[0]({ t: "live", m: [], b: [], f: 0, r: false });
    const bars = [...sheet().querySelectorAll<HTMLElement>(".ch .mtr i")];
    expect(bars.map((b) => b.style.width)).toEqual(["100%", "0%"]);
    meters = [0.0316];
    live[0]({ t: "live", m: [], b: [], f: 0, r: false });
    expect(parseFloat(bars[0].style.width)).toBeCloseTo(44.4, 0);
    expect(bars[1].style.width).toBe("0%");
  });
});

describe("format, pre-roll, split, file name", () => {
  it("segmented controls and toggle", () => {
    ed.open("presets");
    segBtn("32 FLOAT").click();
    expect(draft().format).toBe("wav32f");
    expect(segBtn("32 FLOAT").classList.contains("on")).toBe(true);
    expect(segBtn("24-BIT").classList.contains("on")).toBe(false);
    segBtn("5 S").click();
    expect(draft().preroll_secs).toBe(5);
    expect(sheet().textContent).not.toContain("One file per track");
    segBtn("MULTITRACK").click();
    const tg = sheet().querySelector<HTMLElement>(".toggle")!;
    tg.click();
    expect(draft().split_tracks).toBe(true);
    expect(sheet().querySelector(".preview")!.textContent).toMatch(/ \/ 01 …wav$/);
  });

  it("file name tokens and preview", () => {
    ed.open("presets");
    const fn = sheet().querySelector<HTMLInputElement>("input.mono")!;
    type(fn, "Gig ");
    fn.setSelectionRange(4, 4);
    const tok = [...sheet().querySelectorAll<HTMLElement>("code")].find((c) => c.textContent === "{take}")!;
    const e = new MouseEvent("mousedown", { cancelable: true });
    tok.dispatchEvent(e);
    expect(e.defaultPrevented).toBe(true);
    expect(fn.value).toBe("Gig {take}");
    expect(draft().filename).toBe("Gig {take}");
    expect(sheet().querySelector(".preview")!.textContent).toBe("Gig 01.wav");
    Object.defineProperty(fn, "selectionStart", { value: null });
    tok.dispatchEvent(new MouseEvent("mousedown"));
    expect(fn.value).toBe("Gig {take}{take}");
  });
});

describe("destinations", () => {
  const cards = () => [...sheet().querySelectorAll<HTMLElement>(".dcard")];
  const pill = (n = 0) => cards()[n].querySelector(".dpill-text")!;
  const tiles = () => [...sheet().querySelectorAll<HTMLElement>(".ktile")];
  const share = (o: any = {}) => preset({ destinations: [dest({ id: "s1", kind: "share", label: "NAS", path: "", smb_url: "smb://me@NAS.local/Music/Band%20One/Takes", ...o })] });

  it("status, add by kind, edit, power, remove", () => {
    boot(state({ dests: [{ id: "d1", label: "Local", path: "/music", state: "error", message: "Folder not found", free_bytes: 0 }] }));
    ed.open("presets");
    expect(pill().textContent).toBe("Folder not found");
    expect(pill().className).toBe("dpill-text err");
    expect(cards()[0].dataset.state).toBe("error");
    expect(cards()[0].className).toBe("dcard kind-folder");
    expect(cards()[0].querySelector(".dkind")!.textContent).toBe("LOCAL FOLDER");
    expect(tiles().length).toBe(0);
    sheet().querySelector<HTMLElement>(".dadd")!.click();
    expect(tiles().map((t) => t.querySelector(".ktile-title")!.textContent)).toEqual(["LOCAL FOLDER", "NETWORK SHARE", "TELEGRAM"]);
    expect(sheet().querySelector(".dadd")).toBeNull();
    press("CANCEL");
    expect(tiles().length).toBe(0);
    expect(draft().destinations.length).toBe(1);
    sheet().querySelector<HTMLElement>(".dadd")!.click();
    tiles()[1].click();
    expect(draft().destinations[1]).toMatchObject({ kind: "share", label: "NAS", path: "", smb_url: "", enabled: true });
    expect(tiles().length).toBe(0, "picker closes after a pick");
    expect(hint()).toContain("Unsaved");
    expect(pill(1).textContent).toBe("Checking…");
    expect(cards()[1].dataset.state).toBe("checking");
    sheet().querySelector<HTMLElement>(".dadd")!.click();
    tiles()[1].click();
    expect(draft().destinations[2].label).toBe("NAS 2");
    const name = cards()[1].querySelector<HTMLInputElement>(".dname")!;
    expect(name.placeholder).toBe("NAS");
    type(name, "Big NAS");
    type(cards()[1].querySelector<HTMLInputElement>(".input.mono")!, "smb://nas/music");
    expect(draft().destinations[1]).toMatchObject({ label: "Big NAS", smb_url: "smb://nas/music" });
    cards()[1].querySelector<HTMLElement>(".dswitch")!.click();
    expect(draft().destinations[1].enabled).toBe(false);
    expect(cards()[1].dataset.state).toBe("off");
    expect(cards()[1].querySelector(".dswitch")!.className).toBe("dswitch");
    expect(pill(1).textContent).toBe("Off");
    cards()[1].querySelector<HTMLElement>(".dswitch")!.click();
    expect(cards()[1].querySelector(".dswitch")!.className).toBe("dswitch on");
    cards()[2].querySelector<HTMLElement>(".key.red")!.click();
    cards()[1].querySelector<HTMLElement>(".key.red")!.click();
    cards()[0].querySelector<HTMLElement>(".key.red")!.click();
    expect(draft().destinations.length).toBe(0);
    // empty: the picker is right there, no cancel
    expect(tiles().length).toBe(3);
    expect(keys().some((k) => k.textContent === "CANCEL")).toBe(false);
    expect(sheet().textContent).toContain("takes go to your Music folder for now");
    tiles()[0].click();
    expect(draft().destinations[0]).toMatchObject({ kind: "folder", label: "Local" });
  });

  it("picker resets when another preset is picked", () => {
    boot(state({ presets: [preset(), preset({ id: "p2", name: "Two" })] }));
    ed.open("presets");
    sheet().querySelector<HTMLElement>(".dadd")!.click();
    expect(tiles().length).toBe(3);
    [...sheet().querySelectorAll<HTMLElement>(".plist-item")][1].click();
    expect(tiles().length).toBe(0);
  });

  it("mount command is tucked away until asked for", () => {
    ed.open("presets");
    const adv = sheet().querySelector<HTMLElement>(".adv")!;
    expect(adv.style.display).toBe("none");
    const link = sheet().querySelector<HTMLElement>(".linkish")!;
    link.click();
    expect(adv.style.display).toBe("");
    expect(link.style.display).toBe("none");
    type(adv.querySelector("input")!, "mount /mnt/nas");
    expect(draft().destinations[0].mount_cmd).toBe("mount /mnt/nas");
  });

  it("old folder+share combos keep their share field", () => {
    boot(state({ presets: [preset({ destinations: [dest({ smb_url: "smb://nas/a" })] })] }));
    ed.open("presets");
    const adv = sheet().querySelector<HTMLElement>(".adv")!;
    expect(adv.style.display).toBe("");
    expect(adv.textContent).toContain("CONNECT THIS SHARE FIRST");
    type(adv.querySelectorAll("input")[1], "smb://nas/b");
    expect(draft().destinations[0].smb_url).toBe("smb://nas/b");
  });

  it("share address turns into crumbs, live", () => {
    boot(state({ presets: [share()], dests: [] }));
    ed.open("presets");
    const crumbs = () => [...sheet().querySelectorAll(".crumb")].map((c) => [c.className, c.querySelector("small")!.textContent, c.lastChild!.textContent]);
    expect(crumbs()).toEqual([
      ["crumb host", "SERVER", "nas.local"],
      ["crumb", "SHARE", "Music"],
      ["crumb", "FOLDER", "Band One"],
      ["crumb", "FOLDER", "Takes"],
    ]);
    expect(sheet().querySelectorAll(".crumb-sep").length).toBe(3);
    expect(sheet().textContent).toContain("Keychain");
    expect(sheet().querySelector(".adv")!.getAttribute("style")).toBe("display:none");
    const url = sheet().querySelector<HTMLInputElement>(".dcard-body .input.mono")!;
    expect(url.placeholder).toBe("smb://you@nas.local/Music/Band");
    type(url, "nas/music");
    expect(sheet().querySelector(".crumb-bad")!.textContent).toContain("Start with smb://");
    type(url, "  ");
    expect(sheet().querySelector(".crumb-empty")).not.toBeNull();
    type(url, "smb://nas/Music");
    expect(crumbs().length).toBe(2);
    expect(sheet().querySelector(".dkind")!.textContent).toBe("NETWORK SHARE");
  });

  it("share shows where takes land once found", () => {
    boot(state({ presets: [share({ mount_cmd: "x" })], dests: [{ id: "s1", label: "NAS", path: "", state: "error", message: "Share not connected", free_bytes: 0 }] }));
    ed.open("presets");
    const lands = sheet().querySelector<HTMLElement>(".lands")!;
    expect(lands.className).toBe("lands hidden");
    expect(sheet().querySelector(".adv")!.getAttribute("style")).toBe("");
    ed.update(state({ presets: [share()], dests: [{ id: "s1", label: "NAS", path: "/Volumes/Music/Band One/Takes", state: "ready", message: "Ready", free_bytes: 1e12 }] }));
    expect(lands.className).toBe("lands");
    expect(lands.querySelector("b")!.textContent).toBe("/Volumes/Music/Band One/Takes");
    expect(pill().textContent).toBe("Ready · 931 GB free");
  });

  it("kinds from a newer version are kept but explained", () => {
    boot(state({ presets: [preset({ destinations: [dest({ kind: "unsupported", label: "" })] })], dests: [] }));
    ed.open("presets");
    expect(sheet().querySelector(".dkind")!.textContent).toBe("NEWER KIND");
    expect(sheet().querySelector<HTMLInputElement>(".dname")!.placeholder).toBe("Destination");
    expect(sheet().textContent).toContain("Made by a newer version");
    expect(sheet().querySelector(".dcard-body input")).toBeNull();
  });

  it("inactive presets aren't checked, linux hints", () => {
    boot(state({ platform: "linux", presets: [preset(), preset({ id: "p2", name: "Two", destinations: [dest({ id: "x" }), dest({ id: "y", kind: "share", smb_url: "smb://h/s" })] })] }));
    ed.open("presets");
    expect(sheet().querySelector<HTMLInputElement>(".dest-path input")!.placeholder).toBe("/media/usb/band");
    [...sheet().querySelectorAll<HTMLElement>(".plist-item")][1].click();
    expect(pill().textContent).toBe("Checked when this preset is in use");
    expect(cards()[0].dataset.state).toBe("idle");
    expect(sheet().textContent).toContain("keyring (gio)");
    boot(state({ dests: [] }));
    ed.open("presets");
    expect(pill().textContent).toBe("Checking…");
    expect(sheet().querySelector<HTMLInputElement>(".dest-path input")!.placeholder).toBe("/Volumes/USB/Band");
  });

  it("status LEDs update live", () => {
    ed.open("presets");
    const ds = (st: any, msg = "Share not responding") => state({ dests: [{ id: "d1", label: "Local", path: "/music", state: st, message: msg, free_bytes: 2e9 }] });
    ed.update(ds("error"));
    expect(pill().textContent).toBe("Share not responding");
    ed.update(ds("mounting", "Connecting share"));
    expect(cards()[0].querySelector(".led")!.className).toContain("amber blink");
    expect(cards()[0].dataset.state).toBe("mounting");
    ed.update(ds("ready"));
    expect(pill().textContent).toBe("Ready · 1.9 GB free");
    expect(pill().className).toBe("dpill-text");
    ed.update(state({ dests: [] }));
    // switched off: live status doesn't light it back up
    cards()[0].querySelector<HTMLElement>(".dswitch")!.click();
    ed.update(ds("ready"));
    expect(pill().textContent).toBe("Off");
    // another preset's draft: LEDs left alone
    boot(state({ presets: [preset(), preset({ id: "p2", name: "Two" })] }));
    ed.open("presets");
    [...sheet().querySelectorAll<HTMLElement>(".plist-item")][1].click();
    ed.update({ ...ds("error"), presets: [preset(), preset({ id: "p2", name: "Two" })] });
    expect(pill().textContent).toBe("Checked when this preset is in use");
  });

  it("browse picks a folder, cancel keeps the old one", async () => {
    ed.open("presets");
    let start = "";
    pick = async (s) => { start = s; return "/Volumes/USB"; };
    press("BROWSE");
    await vi.waitFor(() => expect(draft().destinations[0].path).toBe("/Volumes/USB"));
    expect(sheet().querySelector<HTMLInputElement>(".dest-path input")!.value).toBe("/Volumes/USB");
    expect(start).toBe("/music");
    pick = async () => null;
    press("BROWSE");
    await Promise.resolve();
    expect(draft().destinations[0].path).toBe("/Volumes/USB");
    type(sheet().querySelector<HTMLInputElement>(".dest-path input")!, "");
    pick = async (s) => { start = s; return null; };
    press("BROWSE");
    expect(start).toBe("/Users/me/Music/PracticeSpaceRecorder");
  });
});

describe("setup tab", () => {
  it("settings are sent right away", () => {
    ed.open("setup");
    const toggles = [...sheet().querySelectorAll<HTMLElement>(".toggle")];
    toggles[0].click();
    toggles[1].click();
    const port = sheet().querySelector<HTMLInputElement>("input[type=number]")!;
    type(port, "9000");
    port.dispatchEvent(new Event("change"));
    expect(sent).toEqual([
      { cmd: "save_settings", settings: { fullscreen: false } },
      { cmd: "save_settings", settings: { remote_enabled: false } },
      { cmd: "save_settings", settings: { port: 9000 } },
    ]);
    expect(sheet().querySelector(".qr")!.innerHTML).toBe("<svg></svg>");
    expect([...sheet().querySelectorAll(".url")].map((u) => u.textContent)).toEqual(["http://192.168.1.5:7878", "http://studio.local:7878"]);
    expect(sheet().textContent).toContain("None right now");
    press("QUIT APP");
    expect(sent.pop()).toEqual({ cmd: "quit" });
    press("DONE");
    expect(ed.isOpen()).toBe(false);
  });

  it("rebuilds when devices connect, dims when remote is off", () => {
    ed.open("setup");
    ed.update(state({ remote: { ...state().remote, clients: 2 } }));
    expect(sheet().textContent).toContain("2 devices");
    ed.update(state({ remote: { ...state().remote, clients: 1 } }));
    expect(sheet().textContent).toContain("1 device");
    ed.update(state({ remote: { ...state().remote, clients: 1, urls: ["http://10.0.0.2:7878"] } }));
    expect(sheet().querySelector(".url")!.textContent).toBe("http://10.0.0.2:7878");
    ed.update(state({ remote: { ...state().remote, clients: 1, urls: ["http://10.0.0.2:7878"] } }));
    boot(state({ settings: { ...state().settings, remote_enabled: false } }));
    ed.open("setup");
    expect(sheet().querySelector<HTMLElement>(".remote-box")!.getAttribute("style")).toContain("opacity:.4");
  });
});

describe("TEST button", () => {
  const status = () => sheet().querySelector(".dpill-text")!;
  const card = () => sheet().querySelector<HTMLElement>(".dcard")!;
  it("tests the unsaved draft and shows the verdict", async () => {
    ed.open("presets");
    type(sheet().querySelector<HTMLInputElement>(".dest-path input")!, "/Volumes/Oliver/Test");
    let asked: any = null;
    let finish!: (r: any) => void;
    tester = (d) => { asked = d; return new Promise((r) => (finish = r)); };
    press("TEST");
    expect(asked).toMatchObject({ kind: "folder", path: "/Volumes/Oliver/Test" });
    expect(keyBtn("TEST").className).toBe("key test testing");
    expect(status().textContent).toContain("Testing");
    expect(card().dataset.state).toBe("checking");
    expect(card().querySelector(".led")!.className).toContain("amber blink");
    press("TEST"); // busy: ignored
    // live status must not overwrite a running test
    ed.update(state({ dests: [{ id: "d1", label: "Local", path: "/music", state: "error", message: "Share not connected", free_bytes: 0 }] }));
    expect(status().textContent).toContain("Testing");
    finish({ ok: true, msg: "", path: "/Volumes/Oliver/Test", free: 2e12 });
    await vi.waitFor(() => expect(keyBtn("TEST").className).toBe("key test ok"));
    expect(status().textContent).toBe("Works · 1.8 TB free · /Volumes/Oliver/Test");
    expect(status().className).toBe("dpill-text good");
    expect(card().dataset.state).toBe("ready");
    ed.update(state({ dests: [{ id: "d1", label: "Local", path: "/music", state: "error", message: "Share not connected", free_bytes: 0 }] }));
    expect(status().textContent).toContain("Works", "result stays a while");
  });

  it("failure, and live status takes over again later", async () => {
    const now = vi.spyOn(Date, "now");
    now.mockReturnValue(1000);
    ed.open("presets");
    tester = async () => ({ ok: false, msg: "Not writable: Permission denied", path: "", free: 0 });
    press("TEST");
    await vi.waitFor(() => expect(keyBtn("TEST").className).toBe("key test fail"));
    expect(status().textContent).toBe("Not writable: Permission denied");
    expect(status().className).toBe("dpill-text err");
    expect(card().dataset.state).toBe("error");
    expect(card().querySelector(".led")!.className).toContain("red");
    now.mockReturnValue(1000 + 16000);
    ed.update(state());
    expect(status().textContent).toBe("Ready · 466 GB free");
    now.mockRestore();
  });

  it("destructive keys are red", () => {
    ed.open("presets");
    expect(keyBtn("REMOVE").className).toBe("key red");
    expect(keyBtn("DELETE").className).toBe("key red");
  });
});

describe("format", () => {
  const fmt = () => [...sheet().querySelectorAll<HTMLElement>(".fmt-row")][0];
  const btn = (label: string) => [...fmt().querySelectorAll<HTMLElement>("button")].find((b) => b.textContent === label)!;
  const fhint = () => fmt().parentElement!.querySelector(".hint")!.textContent;

  it("WAV or MP3 at a bitrate, with what it costs", () => {
    ed.open("presets");
    expect(fmt().querySelector(".rates")!.className).toContain("hidden");
    expect(fhint()).toBe("As recorded, about 165 MB per 10 minutes.");
    btn("MP3").click();
    expect(draft().destinations[0].encoding).toBe("mp3");
    expect(fmt().querySelector(".rates")!.className).not.toContain("hidden");
    expect(fhint()).toBe("About 14 MB per 10 minutes.");
    btn("320").click();
    expect(draft().destinations[0].bitrate).toBe(320);
    expect(fhint()).toBe("About 23 MB per 10 minutes.");
    expect(hint()).toContain("Unsaved");
    btn("WAV").click();
    expect(draft().destinations[0].encoding).toBe("wav");
  });

  it("multitrack notes: mixes, split files, bit depths", () => {
    const multi = (o: any = {}) => preset({ mode: "multitrack", channels: [1, 2, 3, 4], format: "wav16", ...o });
    boot(state({ presets: [multi({ destinations: [dest({ encoding: "mp3" })] })] }));
    ed.open("presets");
    expect(fhint()).toBe("About 14 MB per 10 minutes. More than two tracks become a stereo mix.");
    boot(state({ presets: [multi({ split_tracks: true, destinations: [dest({ encoding: "mp3" })] })] }));
    ed.open("presets");
    expect(fhint()).toBe("About 27 MB per 10 minutes.", "4 mono files");
    boot(state({ presets: [multi({ split_tracks: true, format: "wav32f" })] }));
    ed.open("presets");
    expect(fhint()).toBe("As recorded, about 439 MB per 10 minutes.");
    boot(state({ presets: [preset({ mode: "mono", channels: [1] })] }));
    ed.open("presets");
    expect(fhint()).toBe("As recorded, about 82 MB per 10 minutes.");
  });
});

describe("telegram", () => {
  const tg = (o: any = {}) => dest({ id: "t1", kind: "telegram", label: "Band chat", path: "", encoding: "mp3", ...o });
  const withTg = (o: any = {}, st: any = []) => boot(state({ presets: [preset({ destinations: [tg(o)] })], dests: st }));
  const tokenIn = () => sheet().querySelector<HTMLInputElement>("input[type=password]")!;
  const chip = () => sheet().querySelector(".chat-chip")!;
  const chats = () => [...sheet().querySelectorAll<HTMLElement>(".chat")];
  const msg = () => sheet().querySelector(".chat-msg")!;

  it("fresh: steps, write-only token", () => {
    withTg();
    ed.open("presets");
    expect(sheet().querySelector(".dkind")!.textContent).toBe("TELEGRAM");
    expect(sheet().querySelectorAll(".tg-steps li").length).toBe(3);
    expect(tokenIn().placeholder).toBe("123456789:AAH4x…");
    expect(chip().className).toBe("chat-chip empty");
    expect(chip().textContent).toBe("No chat picked yet");
    type(tokenIn(), " 123:SECRETabcd ");
    expect(draft().destinations[0].token).toBe("123:SECRETabcd");
    type(tokenIn(), "");
    expect(draft().destinations[0].token).toBe("", "cleared: back to what it was");
    expect(sheet().querySelector(".fmt-row")!.parentElement!.querySelector(".hint")!.textContent).toContain("long takes drop to 128 kbps");
    expect(keyBtn("TEST").title).toBe("Say hi in the chat right now");
  });

  it("saved: masked, kept unless replaced, no steps", () => {
    withTg({ token: "••••abcd", chat_id: "-100", chat_name: "Band Chat" }, [{ id: "t1", label: "Band chat", path: "", state: "ready", message: "Ready · Band Chat", free_bytes: 0 }]);
    ed.open("presets");
    expect(sheet().querySelector(".tg-steps")).toBeNull();
    expect(tokenIn().value).toBe("");
    expect(tokenIn().placeholder).toBe("••••abcd");
    expect(sheet().querySelector(".dpill-text")!.textContent).toBe("Ready · Band Chat");
    expect(chip().className).toBe("chat-chip");
    expect(chip().querySelector("b")!.textContent).toBe("Band Chat");
    expect(chip().querySelector("small")!.textContent).toBe("-100");
    type(tokenIn(), "999:NEW");
    type(tokenIn(), "  ");
    expect(draft().destinations[0].token).toBe("••••abcd");
    // a pasted, unsaved token shows masked after a re-render
    type(tokenIn(), "999:NEWtoken");
    sheet().querySelector<HTMLElement>(".dswitch")!.click();
    expect(tokenIn().placeholder).toBe("••••oken");
  });

  it("find chats: error, nothing yet, pick one", async () => {
    withTg({ token: "••••abcd" });
    ed.open("presets");
    let asked: any = null;
    let finish!: (r: any) => void;
    finder = (d) => { asked = d; return new Promise((r) => (finish = r)); };
    press("FIND CHATS");
    expect(asked).toMatchObject({ id: "t1", token: "••••abcd" });
    expect(keyBtn("FIND CHATS").className).toBe("key test testing");
    expect(msg().textContent).toBe("Asking Telegram…");
    press("FIND CHATS"); // busy: ignored
    finish({ ok: false, msg: "Bot token rejected. Paste it again from @BotFather", bot: "", chats: [] });
    await vi.waitFor(() => expect(msg().className).toBe("chat-msg err"));
    expect(msg().textContent).toContain("token rejected");
    expect(keyBtn("FIND CHATS").className).toBe("key");
    finder = async () => ({ ok: true, msg: "", bot: "@band_bot", chats: [] });
    press("FIND CHATS");
    await vi.waitFor(() => expect(msg().textContent).toContain("@band_bot hasn't seen any chats yet"));
    finder = async () => ({ ok: true, msg: "", bot: "@band_bot", chats: [
      { id: "-100", name: "The Loud Ones", kind: "supergroup" }, { id: "-5", name: "Chan", kind: "channel" }, { id: "7", name: "Oli", kind: "private" }, { id: "8", name: "X", kind: "weird" }] });
    press("FIND CHATS");
    await vi.waitFor(() => expect(chats().length).toBe(4));
    expect(chats().map((c) => c.querySelector(".chat-kind")!.textContent)).toEqual(["GROUP", "CHANNEL", "DIRECT", "WEIRD"]);
    expect(msg().textContent).toBe("Chats @band_bot is in. Pick where takes go:");
    chats()[0].click();
    expect(draft().destinations[0]).toMatchObject({ chat_id: "-100", chat_name: "The Loud Ones", label: "The Loud Ones" });
    expect(chip().querySelector("b")!.textContent).toBe("The Loud Ones");
    expect(sheet().querySelector<HTMLInputElement>(".dname")!.value).toBe("The Loud Ones");
    // a custom name is kept; the picked chat is highlighted next time
    type(sheet().querySelector<HTMLInputElement>(".dname")!, "Us");
    press("FIND CHATS");
    await vi.waitFor(() => expect(chats().length).toBe(4));
    expect(chats()[0].className).toBe("chat on");
    chats()[1].click();
    expect(draft().destinations[0]).toMatchObject({ chat_id: "-5", label: "Us" });
  });

  it("chat id by hand, test says who posted where", async () => {
    withTg({ token: "••••abcd" });
    ed.open("presets");
    const adv = [...sheet().querySelectorAll<HTMLElement>(".adv")].pop()!;
    expect(adv.style.display).toBe("none");
    const link = [...sheet().querySelectorAll<HTMLElement>(".linkish")].pop()!;
    link.click();
    expect(adv.style.display).toBe("");
    type(adv.querySelector("input")!, " @takes ");
    expect(draft().destinations[0]).toMatchObject({ chat_id: "@takes", chat_name: "" });
    expect(chip().querySelector("b")!.textContent).toBe("@takes");
    expect(chip().querySelector("small")!.textContent).toBe("");
    tester = async () => ({ ok: true, msg: "", path: "@band_bot posted to Takes", free: 0 });
    press("TEST");
    await vi.waitFor(() => expect(sheet().querySelector(".dpill-text")!.textContent).toBe("Works · @band_bot posted to Takes"));
  });

  it("ready without free space info shows the monitor's words", () => {
    boot(state({ dests: [{ id: "d1", label: "Local", path: "/music", state: "ready", message: "", free_bytes: 0 }] }));
    ed.open("presets");
    expect(sheet().querySelector(".dpill-text")!.textContent).toBe("Ready");
  });
});
