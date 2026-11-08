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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Destination {
    pub id: String,
    pub label: String,
    /// Folder the takes land in (local folder or mounted share).
    pub path: String,
    /// Optional `smb://user@host/share`; mounted automatically when `path` is missing.
    pub smb_url: String,
    /// Optional shell command that makes `path` available (e.g. `mount /mnt/nas`).
    pub mount_cmd: String,
    pub enabled: bool,
}

impl Default for Destination {
    fn default() -> Self {
        Destination {
            id: new_id("d"),
            label: "Local".into(),
            path: default_music_dir().to_string_lossy().into_owned(),
            smb_url: String::new(),
            mount_cmd: String::new(),
            enabled: true,
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
}

impl Store {
    pub fn load() -> Store {
        let path = config_dir().join("config.json");
        let mut s: Store = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| log::error!("config.json unreadable: {e}")).ok())
            .unwrap_or_default();
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
        write_atomic(&config_dir().join("config.json"), &serde_json::to_vec_pretty(self)?)
    }

    pub fn active(&self) -> &Preset {
        self.presets.iter().find(|p| p.id == self.settings.active_preset).unwrap_or(&self.presets[0])
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
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
    #[test]
    fn template() {
        let now = chrono::Local::now();
        let p = expand_template("{date}/x: {preset} {take}", "Band/Practice", 3, now);
        assert_eq!(p.components().count(), 2);
        assert!(p.to_string_lossy().ends_with("x- Band-Practice 03"));
    }
    #[test]
    fn channels() {
        let mut p = Preset::default();
        p.mode = Mode::Stereo;
        p.channels = vec![3, 4];
        assert_eq!(p.selected_channels(8), vec![2, 3]);
        assert_eq!(p.selected_channels(2), vec![1, 1]);
        p.mode = Mode::Multitrack;
        p.channels = vec![];
        assert_eq!(p.selected_channels(4), vec![0, 1, 2, 3]);
    }
}
