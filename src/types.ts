export type Mode = "mono" | "stereo" | "multitrack";
export type Format = "wav16" | "wav24" | "wav32f";

export interface Destination {
  id: string;
  label: string;
  path: string;
  smb_url: string;
  mount_cmd: string;
  enabled: boolean;
}

export interface Preset {
  id: string;
  name: string;
  device: string;
  sample_rate: number;
  mode: Mode;
  channels: number[];
  track_names: string[];
  format: Format;
  split_tracks: boolean;
  filename: string;
  destinations: Destination[];
  preroll_secs: number;
}

export interface Settings {
  active_preset: string;
  fullscreen: boolean;
  remote_enabled: boolean;
  port: number;
}

export interface DeviceInfo {
  name: string;
  channels: number;
  rates: number[];
  is_default: boolean;
}

export interface AudioStatus {
  state: "opening" | "ready" | "error";
  message: string;
  device: string;
  sample_rate: number;
  device_channels: number;
  selected: number[];
  dropouts: number;
}

export interface DestStatus {
  id: string;
  label: string;
  path: string;
  state: "checking" | "mounting" | "ready" | "error";
  message: string;
  free_bytes: number;
}

export interface TaskView {
  label: string;
  state: "pending" | "busy" | "done" | "failed";
  progress: number;
  message: string;
  out: string;
  next_try: number;
}

export interface JobView {
  id: string;
  take: string;
  preset: string;
  created: number;
  seconds: number;
  tracks: number;
  note: string;
  state: "pending" | "busy" | "done" | "failed";
  tasks: TaskView[];
}

export interface RecView {
  take: string;
  started_at: number;
  started: boolean;
  stopping: boolean;
  frames: number;
  sample_rate: number;
  tracks: string[];
}

export interface AppState {
  t: "state";
  version: string;
  now: number;
  settings: Settings;
  presets: Preset[];
  active: string;
  tracks: string[];
  devices: DeviceInfo[];
  audio: AudioStatus | null;
  rec: RecView | null;
  dests: DestStatus[];
  spool_free: number;
  queue_open: number;
  jobs: JobView[];
  ready: boolean;
  dest_ready: boolean;
  toast: { id: number; level: "ok" | "error"; msg: string } | null;
  remote: { port: number; urls: string[]; qr: string; clients: number; enabled: boolean };
  music_dir: string;
  platform: string;
}

export interface LiveMsg {
  t: "live";
  m: number[];
  b: number[][];
  f: number;
  r: boolean;
}

export interface HistMsg {
  t: "hist";
  rolling: number[][];
  take: number[][];
  recording: boolean;
  bps: number;
}
