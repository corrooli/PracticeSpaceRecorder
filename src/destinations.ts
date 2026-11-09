// Destination kinds. Each kind brings its own look and fields in the editor; the
// Rust side has the matching DestKind (config.rs) and delivery (jobs.rs).
// A new kind = one entry in KINDS + its body in Editor.destBody.

import type { Destination, DestKind, Encoding } from "./types";

export interface KindInfo {
  kind: DestKind;
  /** Engraved name on tiles and cards. */
  title: string;
  /** One line on the picker tile. */
  blurb: string;
  /** Inline SVG, stroked with currentColor. */
  icon: string;
  /** Name for a fresh destination of this kind. */
  label: string;
  /** Format a fresh destination starts with. */
  encoding: Encoding;
}

const svg = (body: string) => `<svg viewBox="0 0 48 48" aria-hidden="true">${body}</svg>`;

export const ICONS = {
  folder: svg('<path d="M5 15a4 4 0 0 1 4-4h9.5l4 4.5H39a4 4 0 0 1 4 4V35a4 4 0 0 1-4 4H9a4 4 0 0 1-4-4z"/><path d="M5 21h38"/><path d="M17 30h14" class="accent"/>'),
  share: svg('<rect x="9" y="5" width="30" height="27" rx="3.5"/><path d="M15 12.5h12M15 18.5h12M15 24.5h12"/><circle cx="33" cy="12.5" r="1.6" class="accent fill"/><circle cx="33" cy="18.5" r="1.6" class="accent fill"/><circle cx="33" cy="24.5" r="1.6" class="fill"/><path d="M24 32v5.5M10 42h28"/><circle cx="24" cy="42" r="3" class="accent"/>'),
  telegram: svg('<path d="M43 6 5 21.5l12.5 4.8L21 40l7.2-8.6L37.5 38z"/><path d="M17.5 26.3 43 6 23.5 30 21 40" class="accent"/>'),
  unknown: svg('<rect x="7" y="7" width="34" height="34" rx="6"/><path d="M19 19a5 5 0 1 1 7 4.6c-1.3.6-2 1.6-2 3V28"/><circle cx="24" cy="34" r="1.6" class="fill"/>'),
};

export const KINDS: KindInfo[] = [
  { kind: "folder", title: "LOCAL FOLDER", blurb: "This computer, a USB stick or an SSD. Plug in, pick the folder, done.", icon: ICONS.folder, label: "Local", encoding: "wav" },
  { kind: "share", title: "NETWORK SHARE", blurb: "Your NAS or a server over SMB. Connects by itself when it's missing.", icon: ICONS.share, label: "NAS", encoding: "wav" },
  { kind: "telegram", title: "TELEGRAM", blurb: "Straight into the band chat. A bot posts every take, ready to play on any phone.", icon: ICONS.telegram, label: "Band chat", encoding: "mp3" },
];

const UNKNOWN: KindInfo = { kind: "unsupported", title: "NEWER KIND", blurb: "", icon: ICONS.unknown, label: "Destination", encoding: "wav" };

/** MP3 bitrates (kbps), like config::BITRATES. */
export const BITRATES = [128, 160, 192, 224, 256, 320];

export function kindInfo(kind: DestKind): KindInfo {
  return KINDS.find((k) => k.kind === kind) || UNKNOWN;
}

/** A fresh destination of `kind`, named so it doesn't clash with the others. */
export function newDestination(kind: DestKind, existing: Destination[], id: string): Destination {
  const base = kindInfo(kind).label;
  const taken = new Set(existing.map((d) => d.label));
  let label = base;
  for (let n = 2; taken.has(label); n++) label = `${base} ${n}`;
  const info = kindInfo(kind);
  return { id, kind, label, path: "", smb_url: "", mount_cmd: "", enabled: true, encoding: info.encoding, bitrate: 192, token: "", chat_id: "", chat_name: "" };
}

/** Bytes per minute of audio in this format. */
export function bytesPerMinute(enc: Encoding, kbps: number, sampleRate: number, channels: number, bits: number): number {
  return enc === "mp3" ? kbps * 125 * 60 : sampleRate * channels * (bits / 8) * 60;
}

export interface ShareUrl { host: string; share: string; sub: string[] }

/** `smb://user@host/Share/sub/dir` split up, like `ShareUrl::parse` in dest.rs. */
export function parseShareUrl(url: string): ShareUrl | null {
  const m = /^\s*(smb|cifs|afp|nfs):\/\/([^/]*)\/?(.*?)\s*$/i.exec(url);
  if (!m) return null;
  const host = m[2].split("@").pop()!.split(":")[0].toLowerCase();
  const dec = (p: string) => { try { return decodeURIComponent(p); } catch { return p; } };
  const parts = m[3].split("/").filter((p) => p && p !== "." && p !== "..").map(dec);
  if (!host || !parts.length) return null;
  return { host, share: parts[0], sub: parts.slice(1) };
}
