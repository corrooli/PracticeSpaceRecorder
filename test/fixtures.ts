import type { AppState, Preset, Destination, JobView } from "../src/types";

export function dest(o: Partial<Destination> = {}): Destination {
  return { id: "d1", kind: "folder", label: "Local", path: "/music", smb_url: "", mount_cmd: "", enabled: true, ...o };
}

export function preset(o: Partial<Preset> = {}): Preset {
  return {
    id: "p1", name: "Band Practice", device: "", sample_rate: 48000, mode: "stereo", channels: [1, 2], track_names: [],
    format: "wav24", split_tracks: false, filename: "{date}/{preset} take {take}", destinations: [dest()], preroll_secs: 2, ...o,
  };
}

export function job(o: Partial<JobView> = {}): JobView {
  return { id: "j1", take: "Take 1", preset: "Band Practice", created: 0, seconds: 75, tracks: 2, note: "", state: "done",
    tasks: [{ label: "Local", state: "done", progress: 1, message: "Delivered", out: "/music/t.wav", next_try: 0 }], ...o };
}

export function state(o: Partial<AppState> = {}): AppState {
  return {
    t: "state", version: "1.0.0", now: 1_000_000,
    settings: { active_preset: "p1", fullscreen: true, remote_enabled: true, port: 7878 },
    presets: [preset()], active: "p1", tracks: ["Left", "Right"],
    devices: [{ name: "Scarlett 2i2", channels: 2, rates: [44100, 48000], is_default: true }],
    audio: { state: "ready", message: "", device: "Scarlett 2i2", sample_rate: 48000, device_channels: 2, selected: [0, 1], dropouts: 0 },
    rec: null,
    dests: [{ id: "d1", label: "Local", path: "/music", state: "ready", message: "Ready", free_bytes: 500e9 }],
    spool_free: 500e9, queue_open: 0, jobs: [], ready: true, dest_ready: true, toast: null,
    remote: { port: 7878, urls: ["http://192.168.1.5:7878", "http://studio.local:7878"], qr: "<svg></svg>", clients: 0, enabled: true },
    music_dir: "/Users/me/Music/PracticeSpaceRecorder", platform: "macos", ...o,
  };
}
