//! Presets and app settings. Everything is persisted immediately (atomic write)
//! so the app always boots straight into the last state.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Mono,
    Stereo,
    Multitrack,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Wav16,
    Wav24,
    Wav32f,
}

impl Format {
    pub fn bits(self) -> u16 {
        match self {
            Format::Wav16 => 16,
            Format::Wav24 => 24,
            Format::Wav32f => 32,
        }
    }
    pub fn is_float(self) -> bool {
        matches!(self, Format::Wav32f)
    }
}

/// What a destination is. Every kind has its own fields in the UI and its own
/// way to check, test and deliver (see `dest::locate`, `jobs::deliver`). New
/// kinds are added here first.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DestKind {
    /// A folder on this machine: internal disk, USB stick, an already mounted share.
    #[default]
    Folder,
    /// A network share, found (and connected) from `smb_url`.
    Share,
    /// A Telegram chat, posted to by a bot (`token`, `chat_id`).
    Telegram,
    /// Written by a newer version of the app. Kept as is, never delivered to.
    #[serde(other)]
    Unsupported,
}

/// What gets written to a destination.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// WAV in the preset's bit depth, tracks as recorded.
    #[default]
    Wav,
    /// MP3 at `bitrate`. More than two tracks become a stereo mix unless split.
    Mp3,
}

/// MP3 bitrates offered in the UI (kbps).
pub const BITRATES: [u32; 6] = [320, 256, 224, 192, 160, 128];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Destination {
    pub id: String,
    pub kind: DestKind,
    pub label: String,
    /// Folder: where takes land.
    pub path: String,
    /// Share: `smb://user@host/share/folder`. On a Folder it only auto-mounts (old configs).
    pub smb_url: String,
    /// Optional shell command that makes `path` available (e.g. `mount /mnt/nas`).
    pub mount_cmd: String,
    pub enabled: bool,
    pub encoding: Encoding,
    /// MP3 bitrate in kbps (one of [`BITRATES`]).
    pub bitrate: u32,
    /// Telegram bot token. Write-only: clients only ever see [`mask_token`].
    pub token: String,
    /// Telegram chat: numeric id, or `@channelname`.
    pub chat_id: String,
    /// Telegram chat title, shown in the UI.
    pub chat_name: String,
}

/// What clients see of a token: enough to recognize it, useless to copy.
pub fn mask_token(t: &str) -> String {
    let t = t.trim();
    if t.is_empty() {
        return String::new();
    }
    let tail: String = t.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("\u{2022}\u{2022}\u{2022}\u{2022}{tail}")
}

impl Destination {
    /// Enabled and pointing somewhere this version can deliver to.
    pub fn usable(&self) -> bool {
        self.enabled
            && match self.kind {
                DestKind::Folder => !self.path.trim().is_empty() || !self.smb_url.trim().is_empty(),
                DestKind::Share => !self.smb_url.trim().is_empty(),
                DestKind::Telegram => !self.token.trim().is_empty() && !self.chat_id.trim().is_empty(),
                DestKind::Unsupported => false,
            }
    }

    /// Configs from before kinds existed: only a share URL means a share.
    pub fn normalize(&mut self) {
        if self.kind == DestKind::Folder && self.path.trim().is_empty() && !self.smb_url.trim().is_empty() {
            self.kind = DestKind::Share;
        }
        if !BITRATES.contains(&self.bitrate) {
            self.bitrate = 192;
        }
    }

    /// The copy clients get: the token masked.
    pub fn redacted(&self) -> Destination {
        Destination { token: mask_token(&self.token), ..self.clone() }
    }

    /// Clients send back an empty or masked token ("keep the one you have"):
    /// put the real one back from the destination with the same id.
    pub fn restore_token(&mut self, known: &[Preset]) {
        let t = self.token.trim();
        if !t.is_empty() && !t.starts_with('\u{2022}') {
            self.token = t.to_string();
            return;
        }
        self.token = known
            .iter()
            .flat_map(|p| &p.destinations)
            .find(|d| d.id == self.id && !d.token.is_empty())
            .map(|d| d.token.clone())
            .unwrap_or_default();
    }

    /// What to call it in the UI.
    pub fn display_name(&self) -> String {
        if !self.label.trim().is_empty() {
            self.label.clone()
        } else if self.kind == DestKind::Telegram {
            if self.chat_name.trim().is_empty() { "Telegram".into() } else { self.chat_name.clone() }
        } else if !self.path.trim().is_empty() {
            self.path.clone()
        } else {
            self.smb_url.clone()
        }
    }
}

impl Default for Destination {
    fn default() -> Self {
        Destination {
            id: new_id("d"),
            kind: DestKind::Folder,
            label: "Local".into(),
            path: default_music_dir().to_string_lossy().into_owned(),
            smb_url: String::new(),
            mount_cmd: String::new(),
            enabled: true,
            encoding: Encoding::Wav,
            bitrate: 192,
            token: String::new(),
            chat_id: String::new(),
            chat_name: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Preset {
    pub id: String,
    pub name: String,
    /// Input device name. Empty = system default input.
    pub device: String,
    pub sample_rate: u32,
    pub mode: Mode,
    /// 1-based device channels. Mono uses [0], stereo [0..2], multitrack all.
    pub channels: Vec<u16>,
    /// Optional names, parallel to `channels`.
    pub track_names: Vec<String>,
    pub format: Format,
    /// Multitrack: one mono file per track (in a folder) instead of one interleaved file.
    pub split_tracks: bool,
    /// Template: {preset} {date} {time} {take} {year} {month} {day}. May contain '/'.
    pub filename: String,
    pub destinations: Vec<Destination>,
    /// Audio kept from *before* you hit space. Musicians are late. It's fine.
    pub preroll_secs: f32,
}

impl Default for Preset {
    fn default() -> Self {
        Preset {
            id: new_id("p"),
            name: "Band Practice".into(),
            device: String::new(),
            sample_rate: 48000,
            mode: Mode::Stereo,
            channels: vec![1, 2],
            track_names: vec![],
            format: Format::Wav24,
            split_tracks: false,
            filename: "{date}/{date} {time} {preset} take {take}".into(),
            destinations: vec![Destination::default()],
            preroll_secs: 2.0,
        }
    }
}

impl Preset {
    /// Selected channels as 0-based indices, validated against the device width.
    pub fn selected_channels(&self, device_channels: u16) -> Vec<usize> {
        let dc = device_channels.max(1) as usize;
        let clamp = |c: u16| (c.max(1) as usize - 1).min(dc - 1);
        match self.mode {
            Mode::Mono => vec![clamp(*self.channels.first().unwrap_or(&1))],
            Mode::Stereo => {
                let l = clamp(*self.channels.first().unwrap_or(&1));
                let r = clamp(*self.channels.get(1).unwrap_or(&((l as u16) + 2)));
                vec![l, r]
            }
            Mode::Multitrack => {
                let mut v: Vec<usize> = self.channels.iter().map(|&c| c as usize).filter(|&c| c >= 1 && c <= dc).map(|c| c - 1).collect();
                v.dedup();
                if v.is_empty() {
                    v = (0..dc).collect();
                }
                v
            }
        }
    }

    pub fn track_name(&self, idx: usize, device_ch: usize) -> String {
        self.track_names
            .get(idx)
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| match self.mode {
                Mode::Mono => "Mono".into(),
                Mode::Stereo => if idx == 0 { "Left".into() } else { "Right".into() },
                Mode::Multitrack => format!("In {}", device_ch + 1),
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub active_preset: String,
    pub fullscreen: bool,
    pub remote_enabled: bool,
    pub port: u16,
    /// Take counter reset daily.
    pub take_date: String,
    pub take_counter: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            active_preset: String::new(),
            fullscreen: true,
            remote_enabled: true,
            port: 7878,
            take_date: String::new(),
            take_counter: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Store {
    pub settings: Settings,
    pub presets: Vec<Preset>,
    /// Where this store lives on disk. Not part of the file itself.
    #[serde(skip)]
    pub path: PathBuf,
}

impl Store {
    /// Load `path`, repairing whatever is missing. Never fails: a broken file
    /// is kept next to it as `config.json.broken` and the app boots on defaults.
    pub fn load(path: &Path) -> Store {
        let mut s: Store = match fs::read(path) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("{} unreadable: {e}", path.display());
                    let _ = fs::copy(path, path.with_extension("json.broken"));
                    Store::default()
                }
            },
            Err(_) => Store::default(),
        };
        s.path = path.to_path_buf();
        for d in s.presets.iter_mut().flat_map(|p| p.destinations.iter_mut()) {
            d.normalize();
        }
        if s.presets.is_empty() {
            s.presets.push(Preset::default());
        }
        if !s.presets.iter().any(|p| p.id == s.settings.active_preset) {
            s.settings.active_preset = s.presets[0].id.clone();
        }
        let _ = s.save();
        s
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(&self.path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn active(&self) -> &Preset {
        self.presets.iter().find(|p| p.id == self.settings.active_preset).unwrap_or(&self.presets[0])
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut name = path.file_name().ok_or_else(|| anyhow::anyhow!("no file name: {}", path.display()))?.to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    {
        // fsync before rename: after a power cut you get the old file or the new
        // one, never an empty one.
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        // presets hold bot tokens: owner only (best effort, shares may refuse)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
        }
        f.write_all(data)?;
        sync_file(&f)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// fsync, without failing on file systems that can't. SMB shares on macOS reject
/// `F_FULLFSYNC` (what `sync_all` does there) with ENOTSUP: every write is
/// already with the server by then, so that is not an error worth losing a take over.
pub fn sync_file(f: &fs::File) -> std::io::Result<()> {
    match f.sync_all() {
        Err(e) if sync_unsupported(&e) => match f.sync_data() {
            Err(e) if sync_unsupported(&e) => Ok(()),
            r => r,
        },
        r => r,
    }
}

/// "This file system doesn't do fsync", as opposed to a real I/O error.
pub fn sync_unsupported(e: &std::io::Error) -> bool {
    #[cfg(target_os = "macos")]
    const CODES: &[i32] = &[45, 102]; // ENOTSUP, EOPNOTSUPP
    #[cfg(not(target_os = "macos"))]
    const CODES: &[i32] = &[22, 95]; // EINVAL (special files), EOPNOTSUPP
    e.kind() == std::io::ErrorKind::Unsupported || e.raw_os_error().is_some_and(|c| CODES.contains(&c))
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("PracticeSpaceRecorder")
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("PracticeSpaceRecorder")
}

pub fn default_music_dir() -> PathBuf {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("PracticeSpaceRecorder")
}

pub fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{prefix}{:x}{:x}", chrono::Utc::now().timestamp_millis(), N.fetch_add(1, Ordering::Relaxed))
}

/// Make a string safe as a single path component.
pub fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '-' } else { c })
        .collect();
    let t = out.trim().trim_matches('.').to_string();
    if t.is_empty() { "untitled".into() } else { t }
}

/// Expand the filename template into relative path components.
pub fn expand_template(tpl: &str, preset: &str, take: u32, now: chrono::DateTime<chrono::Local>) -> PathBuf {
    let tpl = if tpl.trim().is_empty() { "{date} {time} {preset}" } else { tpl };
    let mut p = PathBuf::new();
    for part in tpl.split(['/', '\\']) {
        let s = part
            .replace("{preset}", preset)
            .replace("{date}", &now.format("%Y-%m-%d").to_string())
            .replace("{time}", &now.format("%H-%M-%S").to_string())
            .replace("{take}", &format!("{take:02}"))
            .replace("{year}", &now.format("%Y").to_string())
            .replace("{month}", &now.format("%m").to_string())
            .replace("{day}", &now.format("%d").to_string());
        if !s.trim().is_empty() {
            p.push(sanitize(&s));
        }
    }
    if p.as_os_str().is_empty() {
        p.push("take");
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn preset(mode: Mode, channels: Vec<u16>) -> Preset {
        Preset { mode, channels, ..Preset::default() }
    }

    #[test]
    fn format_bits() {
        assert_eq!((Format::Wav16.bits(), Format::Wav16.is_float()), (16, false));
        assert_eq!((Format::Wav24.bits(), Format::Wav24.is_float()), (24, false));
        assert_eq!((Format::Wav32f.bits(), Format::Wav32f.is_float()), (32, true));
    }

    #[test]
    fn template_all_tokens() {
        let p = expand_template("{year}/{month}/{day}/{date} {time} {preset} {take}", "Band", 7, at(2026, 3, 4, 5, 6, 7));
        let parts: Vec<_> = p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        assert_eq!(parts, ["2026", "03", "04", "2026-03-04 05-06-07 Band 07"]);
    }

    #[test]
    fn template_sanitizes_and_drops_empty_parts() {
        let now = at(2026, 1, 2, 3, 4, 5);
        let p = expand_template("{date}/x: {preset} {take}", "Band/Practice", 3, now);
        assert_eq!(p, PathBuf::from("2026-01-02").join("x- Band-Practice 03"));
        // backslashes split too, empty parts vanish, ".." can't escape
        assert_eq!(expand_template("a\\\\b//c", "", 1, now), PathBuf::from("a").join("b").join("c"));
        assert_eq!(expand_template("../../etc/{preset}", "x", 1, now), PathBuf::from("untitled").join("untitled").join("etc").join("x"));
    }

    #[test]
    fn template_fallbacks() {
        let now = at(2026, 1, 2, 3, 4, 5);
        assert_eq!(expand_template("   ", "P", 1, now), PathBuf::from("2026-01-02 03-04-05 P"));
        assert_eq!(expand_template("/ /", "P", 1, now), PathBuf::from("take"));
        assert_eq!(expand_template("{take}", "P", 123, now), PathBuf::from("123"));
    }

    #[test]
    fn sanitize_cases() {
        assert_eq!(sanitize("a/b\\c:d*e?f\"g<h>i|j"), "a-b-c-d-e-f-g-h-i-j");
        assert_eq!(sanitize("tab\there"), "tab-here");
        assert_eq!(sanitize("  ..hidden.. "), "hidden");
        assert_eq!(sanitize(""), "untitled");
        assert_eq!(sanitize("..."), "untitled");
        assert_eq!(sanitize("Grüße 🎸"), "Grüße 🎸");
    }

    #[test]
    fn mono_channels() {
        assert_eq!(preset(Mode::Mono, vec![3]).selected_channels(8), vec![2]);
        assert_eq!(preset(Mode::Mono, vec![9]).selected_channels(2), vec![1]);
        assert_eq!(preset(Mode::Mono, vec![0]).selected_channels(2), vec![0]);
        assert_eq!(preset(Mode::Mono, vec![]).selected_channels(2), vec![0]);
        assert_eq!(preset(Mode::Mono, vec![1]).selected_channels(0), vec![0]);
    }

    #[test]
    fn stereo_channels() {
        assert_eq!(preset(Mode::Stereo, vec![3, 4]).selected_channels(8), vec![2, 3]);
        assert_eq!(preset(Mode::Stereo, vec![3, 4]).selected_channels(2), vec![1, 1]);
        assert_eq!(preset(Mode::Stereo, vec![5]).selected_channels(8), vec![4, 5]);
        assert_eq!(preset(Mode::Stereo, vec![]).selected_channels(8), vec![0, 1]);
        assert_eq!(preset(Mode::Stereo, vec![1, 2]).selected_channels(1), vec![0, 0]);
    }

    #[test]
    fn multitrack_channels() {
        assert_eq!(preset(Mode::Multitrack, vec![]).selected_channels(4), vec![0, 1, 2, 3]);
        assert_eq!(preset(Mode::Multitrack, vec![2, 2, 4, 9, 0]).selected_channels(4), vec![1, 3]);
        assert_eq!(preset(Mode::Multitrack, vec![9]).selected_channels(2), vec![0, 1]);
        assert_eq!(preset(Mode::Multitrack, vec![]).selected_channels(0), vec![0]);
    }

    #[test]
    fn track_names() {
        let mut p = preset(Mode::Stereo, vec![1, 2]);
        assert_eq!((p.track_name(0, 0), p.track_name(1, 1)), ("Left".into(), "Right".into()));
        p.mode = Mode::Mono;
        assert_eq!(p.track_name(0, 4), "Mono");
        p.mode = Mode::Multitrack;
        assert_eq!(p.track_name(0, 4), "In 5");
        p.track_names = vec!["Kick".into(), "  ".into()];
        assert_eq!(p.track_name(0, 0), "Kick");
        assert_eq!(p.track_name(1, 1), "In 2");
        assert_eq!(p.track_name(5, 7), "In 8");
    }

    #[test]
    fn defaults_are_sane() {
        let p = Preset::default();
        assert_eq!(p.destinations.len(), 1);
        assert!(p.destinations[0].enabled);
        assert!(p.preroll_secs > 0.0);
        let s = Settings::default();
        assert!(s.port >= 1024);
        assert!(new_id("x").starts_with('x'));
        assert_ne!(new_id("x"), new_id("x"));
        assert!(default_music_dir().ends_with("PracticeSpaceRecorder"));
        assert!(config_dir().ends_with("PracticeSpaceRecorder"));
        assert!(data_dir().ends_with("PracticeSpaceRecorder"));
    }

    #[test]
    fn store_missing_file_boots_with_one_preset() {
        let d = TempDir::new("cfg");
        let path = d.join("sub/config.json");
        let s = Store::load(&path);
        assert_eq!(s.presets.len(), 1);
        assert_eq!(s.settings.active_preset, s.presets[0].id);
        assert!(path.exists(), "load writes the repaired file");
        assert_eq!(s.active().id, s.presets[0].id);
    }

    #[test]
    fn store_round_trip() {
        let d = TempDir::new("cfg");
        let path = d.join("config.json");
        let mut s = Store::load(&path);
        let p = Preset { name: "Second".into(), ..Preset::default() };
        s.presets.push(p.clone());
        s.settings.active_preset = p.id.clone();
        s.settings.take_counter = 42;
        s.save().unwrap();
        let back = Store::load(&path);
        assert_eq!(back.presets, s.presets);
        assert_eq!(back.settings, s.settings);
        assert_eq!(back.active().name, "Second");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["presets", "settings"], "store path must not be persisted");
    }

    #[test]
    fn store_repairs_unknown_active_preset() {
        let d = TempDir::new("cfg");
        let path = d.join("config.json");
        std::fs::write(&path, r#"{"settings":{"active_preset":"gone"},"presets":[{"id":"a","name":"A"}]}"#).unwrap();
        let s = Store::load(&path);
        assert_eq!(s.settings.active_preset, "a");
        // missing fields come from defaults
        assert_eq!(s.presets[0].sample_rate, 48000);
        assert_eq!(s.settings.port, 7878);
    }

    #[test]
    fn store_active_falls_back_to_first() {
        let mut s = Store { presets: vec![Preset::default()], ..Default::default() };
        s.settings.active_preset = "nope".into();
        assert_eq!(s.active().id, s.presets[0].id);
    }

    #[test]
    fn broken_config_is_kept_aside() {
        let d = TempDir::new("cfg");
        let path = d.join("config.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let s = Store::load(&path);
        assert_eq!(s.presets.len(), 1);
        assert_eq!(std::fs::read(d.join("config.json.broken")).unwrap(), b"{ this is not json");
        assert!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).is_ok());
    }

    #[test]
    fn save_reports_errors() {
        let d = TempDir::new("cfg");
        std::fs::write(d.join("file"), b"x").unwrap();
        // parent is a file -> cannot create folder
        let s = Store { path: d.join("file/config.json"), ..Default::default() };
        assert!(s.save().is_err());
        assert!(Store::default().save().is_err(), "empty path");
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let d = TempDir::new("cfg");
        let f = d.join("a/b.json");
        write_atomic(&f, b"one").unwrap();
        write_atomic(&f, b"two").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(d.join("a")).unwrap().count(), 1);
        assert!(write_atomic(Path::new("/"), b"x").is_err());
    }

    #[test]
    fn fsync_unsupported_is_not_an_error() {
        use std::io::{Error, ErrorKind};
        assert!(sync_unsupported(&Error::from(ErrorKind::Unsupported)));
        #[cfg(target_os = "macos")]
        assert!(sync_unsupported(&Error::from_raw_os_error(45)), "ENOTSUP from SMB");
        #[cfg(not(target_os = "macos"))]
        assert!(sync_unsupported(&Error::from_raw_os_error(95)));
        assert!(!sync_unsupported(&Error::from_raw_os_error(5)), "EIO is real");
        assert!(!sync_unsupported(&Error::from(ErrorKind::PermissionDenied)));
        let d = TempDir::new("cfg");
        let f = fs::File::create(d.join("x")).unwrap();
        sync_file(&f).unwrap();
    }

    #[test]
    fn destination_kinds() {
        let d: Destination = serde_json::from_str(r#"{"path":"/x"}"#).unwrap();
        assert_eq!(d.kind, DestKind::Folder, "old configs default to folder");
        let mut d: Destination = serde_json::from_str(r#"{"path":"","smb_url":"smb://nas/s"}"#).unwrap();
        d.normalize();
        assert_eq!(d.kind, DestKind::Share, "URL only: a share");
        let mut d: Destination = serde_json::from_str(r#"{"path":"/Volumes/s/Band","smb_url":"smb://nas/s"}"#).unwrap();
        d.normalize();
        assert_eq!(d.kind, DestKind::Folder, "folder + URL keeps its folder");
        assert!(d.usable());
        let d: Destination = serde_json::from_str(r#"{"kind":"carrier_pigeon","path":"/x","coop":"123"}"#).unwrap();
        assert_eq!(d.kind, DestKind::Unsupported, "newer kinds don't break the config");
        assert!(!d.usable());
        let d = Destination { kind: DestKind::Share, path: "/ignored".into(), smb_url: "".into(), ..Default::default() };
        assert!(!d.usable(), "a share needs its URL");
        assert_eq!(serde_json::to_value(DestKind::Share).unwrap(), "share");
    }

    fn tg(token: &str) -> Destination {
        Destination { id: "t1".into(), kind: DestKind::Telegram, label: "".into(), token: token.into(), chat_id: "-100".into(), ..Default::default() }
    }

    #[test]
    fn telegram_destinations() {
        assert!(tg("123:abc").usable());
        assert!(!tg(" ").usable(), "needs a token");
        assert!(!Destination { chat_id: "".into(), ..tg("x") }.usable(), "needs a chat");
        assert_eq!(tg("x").display_name(), "Telegram");
        assert_eq!(Destination { chat_name: "Band Chat".into(), ..tg("x") }.display_name(), "Band Chat");
        assert_eq!(Destination { label: "Band".into(), ..tg("x") }.display_name(), "Band");
        let d: Destination = serde_json::from_str(r#"{"kind":"telegram","bitrate":999}"#).unwrap();
        assert_eq!((d.encoding, d.bitrate), (Encoding::Wav, 999));
        let mut d = d;
        d.normalize();
        assert_eq!(d.bitrate, 192, "unknown bitrates fall back");
        let mut d: Destination = serde_json::from_str(r#"{"encoding":"mp3","bitrate":320}"#).unwrap();
        d.normalize();
        assert_eq!((d.encoding, d.bitrate), (Encoding::Mp3, 320));
    }

    #[test]
    fn tokens_are_masked_and_restored() {
        assert_eq!(mask_token(""), "");
        assert_eq!(mask_token(" 123456:ABCdef "), "\u{2022}\u{2022}\u{2022}\u{2022}Cdef");
        assert_eq!(mask_token("ab"), "\u{2022}\u{2022}\u{2022}\u{2022}ab");
        let real = tg("123456:SECRETxyz9");
        let r = real.redacted();
        assert!(!serde_json::to_string(&r).unwrap().contains("SECRET"));
        assert_eq!(r.token, "\u{2022}\u{2022}\u{2022}\u{2022}xyz9");
        let known = vec![Preset { destinations: vec![tg(""), real.clone()], ..Default::default() }];
        // masked or empty: the stored one comes back
        let mut back = r.clone();
        back.restore_token(&known);
        assert_eq!(back.token, real.token);
        let mut empty = tg("");
        empty.restore_token(&known);
        assert_eq!(empty.token, real.token);
        // a new one replaces it (trimmed)
        let mut fresh = tg(" 999:NEW ");
        fresh.restore_token(&known);
        assert_eq!(fresh.token, "999:NEW");
        // unknown id: a mask can't be turned back into anything
        let mut stranger = Destination { id: "zz".into(), ..r };
        stranger.restore_token(&known);
        assert_eq!(stranger.token, "");
    }

    #[cfg(unix)]
    #[test]
    fn saved_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new("cfg");
        let f = d.join("config.json");
        write_atomic(&f, b"{}").unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn store_load_migrates_destinations() {
        let d = TempDir::new("cfg");
        let path = d.join("config.json");
        std::fs::write(&path, r#"{"presets":[{"id":"a","destinations":[{"id":"x","path":"","smb_url":"smb://nas/s/t"}]}]}"#).unwrap();
        assert_eq!(Store::load(&path).presets[0].destinations[0].kind, DestKind::Share);
    }
}
