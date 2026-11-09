//! Telegram destinations: a bot posts takes to a chat. Bot API over plain HTTPS
//! (ureq, blocking: we run on job and monitor threads anyway).
//!
//! The token is part of every URL, and URLs end up in error messages. Nothing
//! from ureq's errors is passed on verbatim for that reason.

use crate::config::{sanitize, DestKind, Destination, Encoding, BITRATES};
use crate::encode::{analyze, mix_channels, render, Layout, Mp3Writer, Sink, SpoolReader, Tags};
use crate::jobs::{Job, Progress};
use crate::wav::WavWriter;
use serde::Serialize;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;
use std::time::Duration;

/// Bots may upload 50 MB per file. Leave room for tags and form fields.
pub const LIMIT: u64 = 48_000_000;
/// Lowest bitrate we go down to before splitting a take into parts.
pub const FLOOR_KBPS: u32 = 128;

#[cfg(not(test))]
fn api_base() -> String {
    "https://api.telegram.org".into()
}

#[cfg(test)]
thread_local! {
    pub static API_BASE: std::cell::RefCell<String> = std::cell::RefCell::new("http://127.0.0.1:9".into());
}

#[cfg(test)]
fn api_base() -> String {
    API_BASE.with(|b| b.borrow().clone())
}

#[cfg(not(test))]
fn limit() -> u64 {
    LIMIT
}

#[cfg(test)]
thread_local! {
    /// Tests split tiny takes into parts by lowering this.
    pub static TEST_LIMIT: std::cell::Cell<u64> = const { std::cell::Cell::new(LIMIT) };
}

#[cfg(test)]
fn limit() -> u64 {
    TEST_LIMIT.with(|l| l.get())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Chat {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, PartialEq)]
struct ApiError {
    code: u16,
    desc: String,
    migrate_to: Option<i64>,
    retry_after: Option<u64>,
}

/// One file to post.
pub struct Upload<'a> {
    /// `sendAudio` or `sendDocument`.
    pub method: &'a str,
    /// Form field the file goes in: `audio` or `document`.
    pub field: &'a str,
    pub fields: &'a [(&'a str, String)],
    pub path: &'a Path,
    pub filename: &'a str,
}

pub struct Bot {
    base: String,
    agent: ureq::Agent,
}

impl Bot {
    pub fn new(token: &str) -> Bot {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(90))
            .timeout_write(Duration::from_secs(90))
            .build();
        Bot { base: format!("{}/bot{}", api_base(), token.trim()), agent }
    }

    fn call(&self, method: &str, body: &Value) -> Result<Value, ApiError> {
        let r = self.agent.post(&format!("{}/{method}", self.base)).set("Content-Type", "application/json").send_string(&body.to_string());
        parse(r)
    }

    /// The bot's @username.
    pub fn me(&self) -> Result<String, String> {
        let v = self.call("getMe", &json!({})).map_err(|e| human(&e))?;
        Ok(format!("@{}", v["username"].as_str().unwrap_or("bot")))
    }

    /// Chats the bot has heard from lately (Telegram keeps updates for 24 h).
    /// Groups show up once the bot is added, or after any message in them.
    pub fn chats(&self) -> Result<Vec<Chat>, String> {
        let v = self
            .call("getUpdates", &json!({ "allowed_updates": ["message", "channel_post", "my_chat_member"], "timeout": 0 }))
            .map_err(|e| human(&e))?;
        let mut out: Vec<Chat> = Vec::new();
        for u in v.as_array().into_iter().flatten() {
            let (chat, gone) = if let Some(m) = u.get("my_chat_member") {
                let st = m["new_chat_member"]["status"].as_str().unwrap_or("");
                (&m["chat"], st == "left" || st == "kicked")
            } else if let Some(m) = u.get("message").or_else(|| u.get("channel_post")) {
                (&m["chat"], false)
            } else {
                continue;
            };
            let Some(id) = chat["id"].as_i64() else { continue };
            let id = id.to_string();
            out.retain(|c| c.id != id);
            if !gone {
                out.push(Chat { id, name: chat_name(chat), kind: chat["type"].as_str().unwrap_or("").into() });
            }
        }
        out.reverse(); // most recent first
        Ok(out)
    }

    /// Checks token, chat and that the bot is still in it. Returns the chat title.
    pub fn check_chat(&self, chat_id: &str) -> Result<String, String> {
        let v = self.with_chat(chat_id, |c| self.call("getChat", &json!({ "chat_id": c })))?;
        Ok(chat_name(&v))
    }

    pub fn send_message(&self, chat_id: &str, text: &str) -> Result<(), String> {
        self.with_chat(chat_id, |c| self.call("sendMessage", &json!({ "chat_id": c, "text": text }))).map(|_| ())
    }

    /// Upload a file as multipart, streamed from disk, reporting 0..1.
    pub fn send_file(&self, chat_id: &str, up: &Upload, progress: &mut dyn FnMut(f32)) -> Result<(), String> {
        let Upload { method, field, fields, path, filename } = *up;
        self.with_chat(chat_id, |c| {
            let boundary = format!("psr{:016x}", rand_u64());
            let mut head = String::new();
            for (k, v) in std::iter::once(&("chat_id", c.to_string())).chain(fields) {
                head += &format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n");
            }
            let mime = if filename.ends_with(".mp3") { "audio/mpeg" } else { "audio/wav" };
            let fname = filename.replace(['"', '\r', '\n'], "");
            head += &format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{fname}\"\r\nContent-Type: {mime}\r\n\r\n");
            let tail = format!("\r\n--{boundary}--\r\n");
            let f = File::open(path).map_err(|e| ApiError { code: 0, desc: format!("Rendered file missing: {e}"), migrate_to: None, retry_after: None })?;
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            let total = head.len() as u64 + size + tail.len() as u64;
            let body = Counting { inner: Cursor::new(head.into_bytes()).chain(f).chain(Cursor::new(tail.into_bytes())), done: 0, total, step: 0, progress: &mut *progress };
            let r = self
                .agent
                .post(&format!("{}/{method}", self.base))
                .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
                .set("Content-Length", &total.to_string())
                .send(body);
            parse(r)
        })
        .map(|_| ())
    }

    /// Run a call; when a group became a supergroup, run it again with the new id.
    fn with_chat(&self, chat_id: &str, mut f: impl FnMut(&str) -> Result<Value, ApiError>) -> Result<Value, String> {
        match f(chat_id.trim()) {
            Err(ApiError { migrate_to: Some(id), .. }) => f(&id.to_string()).map_err(|e| human(&e)),
            r => r.map_err(|e| human(&e)),
        }
    }
}

struct Counting<'a, R: Read> {
    inner: R,
    done: u64,
    total: u64,
    step: u64,
    progress: &'a mut dyn FnMut(f32),
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.done += n as u64;
        if self.done - self.step >= 256 * 1024 || (n == 0 && self.step != self.done) {
            self.step = self.done;
            (self.progress)(self.done as f32 / self.total.max(1) as f32);
        }
        Ok(n)
    }
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(crate::jobs::now_ms() as u64);
    h.finish()
}

fn chat_name(chat: &Value) -> String {
    if let Some(t) = chat["title"].as_str() {
        return t.into();
    }
    let full = [chat["first_name"].as_str(), chat["last_name"].as_str()].into_iter().flatten().collect::<Vec<_>>().join(" ");
    if !full.is_empty() {
        full
    } else {
        chat["username"].as_str().map(|u| format!("@{u}")).unwrap_or_else(|| chat["id"].to_string())
    }
}

fn parse(r: Result<ureq::Response, ureq::Error>) -> Result<Value, ApiError> {
    let err = |code: u16, desc: String| ApiError { code, desc, migrate_to: None, retry_after: None };
    let body = match r {
        Ok(resp) => resp.into_string(),
        Err(ureq::Error::Status(code, resp)) => {
            let v: Value = resp.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            return Err(ApiError {
                code,
                desc: v["description"].as_str().unwrap_or("").to_string(),
                migrate_to: v["parameters"]["migrate_to_chat_id"].as_i64(),
                retry_after: v["parameters"]["retry_after"].as_u64(),
            });
        }
        // never format the transport error: it contains the URL, and the URL the token
        Err(ureq::Error::Transport(t)) => {
            let msg = match t.kind() {
                ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => "Telegram not reachable (offline?)",
                ureq::ErrorKind::Io => "Connection to Telegram dropped",
                _ => "Network error talking to Telegram",
            };
            return Err(err(0, msg.into()));
        }
    };
    let v: Value = body.ok().and_then(|s| serde_json::from_str(&s).ok()).ok_or_else(|| err(0, "Telegram sent something odd".into()))?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v["result"].clone())
    } else {
        Err(err(v["error_code"].as_u64().unwrap_or(0) as u16, v["description"].as_str().unwrap_or("").into()))
    }
}

/// What the band reads when something goes wrong.
fn human(e: &ApiError) -> String {
    let d = e.desc.to_lowercase();
    match e.code {
        0 => e.desc.clone(),
        401 | 404 => "Bot token rejected. Paste it again from @BotFather".into(),
        400 if d.contains("chat not found") => "Chat not found. Is the bot in that chat?".into(),
        403 if d.contains("kicked") => "The bot was removed from that chat".into(),
        403 if d.contains("blocked") => "The bot was blocked in that chat".into(),
        403 if d.contains("rights") || d.contains("not a member") => "The bot isn't allowed to post in that chat".into(),
        409 => "This bot has a webhook elsewhere. Use a bot of its own for the recorder".into(),
        413 => "File too big for Telegram".into(),
        429 => format!("Telegram asked to slow down, retrying in {} s", e.retry_after.unwrap_or(30)),
        _ if !e.desc.is_empty() => format!("Telegram: {}", e.desc),
        c => format!("Telegram error {c}"),
    }
}

// ----------------------------------------------------------------- delivery

/// How a take goes out: at which bitrate (0 = WAV) and in how many parts.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub kbps: u32,
    pub parts: u32,
    pub frames_per_part: u64,
}

/// Fit a take under the upload limit: MP3 goes down the bitrate ladder to
/// [`FLOOR_KBPS`] first, whatever still doesn't fit is split into equal parts.
pub fn plan(enc: Encoding, kbps: u32, frames: u64, sample_rate: u32, channels: u16, wav_bits: u16) -> Plan {
    let secs = frames as f64 / sample_rate.max(1) as f64;
    let mp3_size = |k: u32| (secs * k as f64 * 125.0 * 1.01) as u64 + 4096;
    let (kbps, size) = match enc {
        Encoding::Wav => (0, frames * channels as u64 * (wav_bits as u64 / 8) + 44),
        Encoding::Mp3 => {
            let k = BITRATES.iter().copied().filter(|&b| b <= kbps.max(FLOOR_KBPS) && b >= FLOOR_KBPS).find(|&b| mp3_size(b) <= limit()).unwrap_or(FLOOR_KBPS.min(kbps.max(FLOOR_KBPS)));
            (k, mp3_size(k))
        }
    };
    let parts = size.div_ceil(limit()).max(1) as u32;
    Plan { kbps, parts, frames_per_part: frames.div_ceil(parts as u64).max(1) }
}

fn clock(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Render the take (stereo mix, chosen format) part by part and post each one.
/// Parts already posted (`sent`) are skipped, so a retry never doubles them up.
pub fn deliver(job: &Job, dest: &Destination, sent: u32, progress: &mut dyn FnMut(Progress)) -> Result<String, String> {
    debug_assert_eq!(dest.kind, DestKind::Telegram);
    let m = &job.meta;
    let n = m.track_names.len().max(1);
    let frames = crate::encode::spool_frames(&job.raw, n)?;
    let ch = mix_channels(n);
    let p = plan(dest.encoding, dest.bitrate, frames, m.sample_rate, ch, m.format.bits());
    // one gain for the whole take, so parts sound the same
    let gain = if dest.master && sent < p.parts {
        Some(analyze(SpoolReader::open(&job.raw, n, 0, frames)?, Layout::Mix, m.sample_rate, &mut |f| progress(Progress::Fraction(0.1 * f)))?)
    } else {
        None
    };
    let pre = if gain.is_some() { 0.1 } else { 0.0 };
    let bot = Bot::new(&dest.token);
    let ext = if p.kbps > 0 { "mp3" } else { "wav" };
    let started = chrono::DateTime::from_timestamp_millis(m.started_at).unwrap_or_default().with_timezone(&chrono::Local);
    let name = sanitize(&m.take_name);
    for i in sent..p.parts {
        let start = i as u64 * p.frames_per_part;
        let len = p.frames_per_part.min(frames.saturating_sub(start));
        let of = if p.parts > 1 { format!(" ({} of {})", i + 1, p.parts) } else { String::new() };
        let tmp = job.raw.with_file_name(format!("{}.{}.{}.{ext}", job.raw.file_stem().unwrap_or_default().to_string_lossy(), sanitize(&dest.id), i));
        let base = pre + (1.0 - pre) * i as f32 / p.parts as f32;
        let share = (1.0 - pre) / p.parts as f32;
        let tags = Tags { title: format!("{}{of}", m.take_name), artist: m.preset_name.clone(), album: m.preset_name.clone(), year: started.format("%Y").to_string(), comment: "PracticeSpaceRecorder".into() };
        let sink: Box<dyn Sink> = if p.kbps > 0 {
            Box::new(Mp3Writer::create(&tmp, m.sample_rate, ch, p.kbps, &tags).map_err(|e| format!("Cannot write: {e}"))?)
        } else {
            Box::new(WavWriter::create(&tmp, m.sample_rate, ch, m.format.bits(), m.format.is_float()).map_err(|e| format!("Cannot write: {e}"))?)
        };
        let rd = SpoolReader::open(&job.raw, n, start, len)?;
        let master = gain.map(|g| crate::master::Master::new(m.sample_rate, ch as usize, g));
        let res = render(rd, Layout::Mix, vec![sink], master, &mut |f| progress(Progress::Fraction(base + share * 0.3 * f))).and_then(|_| {
            let secs = len / m.sample_rate.max(1) as u64;
            let caption = format!("{} \u{b7} {}{of} \u{b7} {} \u{b7} {}", m.preset_name, m.take_name, clock(secs), started.format("%a %-d %b %Y, %H:%M"));
            let file = format!("{name}{of}.{ext}");
            let up = &mut |f: f32| progress(Progress::Fraction(base + share * (0.3 + 0.7 * f)));
            if p.kbps > 0 {
                let fields = [("caption", caption), ("title", tags.title.clone()), ("performer", m.preset_name.clone()), ("duration", secs.to_string())];
                bot.send_file(&dest.chat_id, &Upload { method: "sendAudio", field: "audio", fields: &fields, path: &tmp, filename: &file }, up)
            } else {
                bot.send_file(&dest.chat_id, &Upload { method: "sendDocument", field: "document", fields: &[("caption", caption)], path: &tmp, filename: &file }, up)
            }
        });
        let _ = std::fs::remove_file(&tmp);
        res?;
        progress(Progress::Sent(i + 1));
    }
    let chat = if dest.chat_name.trim().is_empty() { dest.chat_id.clone() } else { dest.chat_name.clone() };
    let how = if p.kbps > 0 { format!("MP3 {}k", p.kbps) } else { "WAV".into() };
    Ok(if p.parts > 1 { format!("Telegram \u{b7} {chat} \u{b7} {how}, {} parts", p.parts) } else { format!("Telegram \u{b7} {chat} \u{b7} {how}") })
}

/// TEST: token, chat and posting rights, by saying hello in the chat.
pub fn test(dest: &Destination) -> Result<String, String> {
    if dest.token.trim().is_empty() {
        return Err("Paste the bot token first".into());
    }
    if dest.chat_id.trim().is_empty() {
        return Err("Pick a chat first (FIND CHATS)".into());
    }
    let bot = Bot::new(&dest.token);
    let me = bot.me()?;
    let chat = bot.check_chat(&dest.chat_id)?;
    bot.send_message(&dest.chat_id, "\u{1f399} PracticeSpaceRecorder says hi. Takes from this destination show up here.")?;
    Ok(format!("{me} posted to {chat}"))
}

/// Live status for the monitor: token and chat still good?
pub fn probe(dest: &Destination) -> Result<String, String> {
    let chat = Bot::new(&dest.token).check_chat(&dest.chat_id)?;
    Ok(format!("Ready \u{b7} {chat}"))
}

#[cfg(test)]
pub mod mock {
    //! A tiny stand-in for api.telegram.org: a thread answering HTTP/1.1 with
    //! canned responses, recording what it was asked.
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    pub struct Req {
        pub path: String,
        pub headers: String,
        pub body: Vec<u8>,
    }

    pub type Reply = Box<dyn Fn(&Req) -> (u16, String) + Send + Sync>;

    pub struct Mock {
        pub reqs: Arc<Mutex<Vec<Req>>>,
    }

    impl Mock {
        /// Starts the server and points this thread's Telegram calls at it.
        pub fn start(reply: impl Fn(&Req) -> (u16, String) + Send + Sync + 'static) -> Mock {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            let reqs = Arc::new(Mutex::new(vec![]));
            let r2 = reqs.clone();
            let reply: Arc<Reply> = Arc::new(Box::new(reply));
            std::thread::spawn(move || {
                for s in l.incoming() {
                    let Ok(mut s) = s else { continue };
                    let reqs = r2.clone();
                    let reply = reply.clone();
                    std::thread::spawn(move || {
                        let mut rd = BufReader::new(s.try_clone().unwrap());
                        loop {
                            let mut line = String::new();
                            if rd.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                            let mut headers = String::new();
                            let mut len = 0usize;
                            loop {
                                let mut h = String::new();
                                rd.read_line(&mut h).unwrap();
                                if h == "\r\n" || h.is_empty() {
                                    break;
                                }
                                if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                                    len = v.trim().parse().unwrap();
                                }
                                headers += &h;
                            }
                            let mut body = vec![0; len];
                            rd.read_exact(&mut body).unwrap();
                            let req = Req { path, headers, body };
                            let (code, text) = reply(&req);
                            reqs.lock().unwrap().push(req);
                            let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}", text.len());
                        }
                    });
                }
            });
            super::API_BASE.with(|b| *b.borrow_mut() = url);
            Mock { reqs }
        }

        pub fn paths(&self) -> Vec<String> {
            self.reqs.lock().unwrap().iter().map(|r| r.path.clone()).collect()
        }
    }

    pub fn ok(result: &str) -> (u16, String) {
        (200, format!(r#"{{"ok":true,"result":{result}}}"#))
    }

    pub fn fail(code: u16, desc: &str) -> (u16, String) {
        (code, format!(r#"{{"ok":false,"error_code":{code},"description":"{desc}"}}"#))
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{fail, ok, Mock};
    use super::*;

    #[test]
    fn plan_lowers_bitrate_then_splits() {
        let sr = 48_000;
        let min = |m: u64| m * 60 * sr as u64;
        // short take: the chosen bitrate
        assert_eq!(plan(Encoding::Mp3, 320, min(10), sr, 2, 24), Plan { kbps: 320, parts: 1, frames_per_part: min(10) });
        // 30 min at 320k = 72 MB: down the ladder until it fits (192k = 43 MB)
        assert_eq!(plan(Encoding::Mp3, 320, min(30), sr, 2, 24).kbps, 192);
        // 60 min: even 128k is 58 MB, so 128k in 2 parts
        let p = plan(Encoding::Mp3, 320, min(60), sr, 2, 24);
        assert_eq!((p.kbps, p.parts, p.frames_per_part), (128, 2, min(30)));
        // never above what was picked, never below the floor
        assert_eq!(plan(Encoding::Mp3, 160, min(1), sr, 2, 24).kbps, 160);
        assert_eq!(plan(Encoding::Mp3, 64, min(1), sr, 2, 24).kbps, 128);
        // WAV: stereo 24 bit is 17 MB a minute, so 10 min = 6 parts
        let p = plan(Encoding::Wav, 192, min(10), sr, 2, 24);
        assert_eq!((p.kbps, p.parts), (0, 4));
        assert_eq!(plan(Encoding::Wav, 192, min(1), sr, 1, 16).parts, 1);
        assert_eq!(plan(Encoding::Wav, 192, 0, sr, 2, 16), Plan { kbps: 0, parts: 1, frames_per_part: 1 });
        // each part really fits
        for m in [1, 20, 45, 90, 180] {
            let p = plan(Encoding::Mp3, 256, min(m), sr, 2, 24);
            assert!((p.frames_per_part as f64 / sr as f64) * p.kbps as f64 * 125.0 * 1.01 < LIMIT as f64, "{m} min");
        }
    }

    #[test]
    fn errors_are_human_and_never_carry_the_token() {
        let e = |code, desc: &str| human(&ApiError { code, desc: desc.into(), migrate_to: None, retry_after: Some(7) });
        assert!(e(401, "Unauthorized").contains("token rejected"));
        assert!(e(404, "Not Found").contains("token rejected"));
        assert!(e(400, "Bad Request: chat not found").contains("Chat not found"));
        assert!(e(403, "Forbidden: bot was kicked from the group chat").contains("removed"));
        assert!(e(403, "Forbidden: bot was blocked by the user").contains("blocked"));
        assert!(e(403, "Bad Request: not enough rights to send audios").contains("isn't allowed"));
        assert!(e(403, "Forbidden: bot is not a member of the channel chat").contains("isn't allowed"));
        assert!(e(409, "Conflict: can't use getUpdates method while webhook is active").contains("webhook"));
        assert!(e(413, "Request Entity Too Large").contains("too big"));
        assert_eq!(e(429, "Too Many Requests"), "Telegram asked to slow down, retrying in 7 s");
        assert_eq!(e(500, "Internal"), "Telegram: Internal");
        assert_eq!(e(502, ""), "Telegram error 502");
        assert_eq!(e(0, "Telegram not reachable (offline?)"), "Telegram not reachable (offline?)");
        // nothing listening: the message must not contain the URL
        API_BASE.with(|b| *b.borrow_mut() = "http://127.0.0.1:9".into());
        let err = Bot::new("123:SECRET").me().unwrap_err();
        assert_eq!(err, "Telegram not reachable (offline?)");
        API_BASE.with(|b| *b.borrow_mut() = "http://nonexistent.invalid".into());
        assert!(!Bot::new("123:SECRET").me().unwrap_err().contains("SECRET"));
    }

    #[test]
    fn me_chats_and_check() {
        let m = Mock::start(|r| match r.path.rsplit('/').next().unwrap() {
            "getMe" => ok(r#"{"id":1,"is_bot":true,"username":"band_bot"}"#),
            "getUpdates" => ok(r#"[
                {"update_id":1,"message":{"chat":{"id":-100,"type":"group","title":"Old Name"}}},
                {"update_id":2,"message":{"chat":{"id":5,"type":"private","first_name":"Oli","last_name":"V"}}},
                {"update_id":3,"my_chat_member":{"chat":{"id":-200,"type":"supergroup","title":"Gone"},"new_chat_member":{"status":"member"}}},
                {"update_id":4,"my_chat_member":{"chat":{"id":-200,"type":"supergroup","title":"Gone"},"new_chat_member":{"status":"kicked"}}},
                {"update_id":5,"channel_post":{"chat":{"id":-300,"type":"channel","username":"takes"}}},
                {"update_id":6,"message":{"chat":{"id":-100,"type":"group","title":"Band Chat"}}},
                {"update_id":7,"edited_message":{"chat":{"id":9}}},
                {"update_id":8,"message":{"chat":{"type":"group"}}},
                {"update_id":9,"message":{"chat":{"id":6,"type":"private"}}}
            ]"#),
            "getChat" => ok(r#"{"id":-100,"type":"group","title":"Band Chat"}"#),
            _ => fail(400, "nope"),
        });
        let b = Bot::new(" 123:abc ");
        assert_eq!(b.me().unwrap(), "@band_bot");
        assert_eq!(m.paths()[0], "/bot123:abc/getMe");
        let c = b.chats().unwrap();
        let names: Vec<(&str, &str, &str)> = c.iter().map(|c| (c.id.as_str(), c.name.as_str(), c.kind.as_str())).collect();
        assert_eq!(names, vec![("6", "6", "private"), ("-100", "Band Chat", "group"), ("-300", "@takes", "channel"), ("5", "Oli V", "private")]);
        assert_eq!(b.check_chat("-100").unwrap(), "Band Chat");
        let body = String::from_utf8(m.reqs.lock().unwrap().last().unwrap().body.clone()).unwrap();
        assert_eq!(body, r#"{"chat_id":"-100"}"#);
    }

    #[test]
    fn odd_answers() {
        let _m = Mock::start(|r| match r.path.rsplit('/').next().unwrap() {
            "getMe" => (200, "not json".into()),
            "getUpdates" => (200, r#"{"ok":false,"error_code":409,"description":"Conflict"}"#.into()),
            "getChat" => (500, "<html>".into()),
            _ => ok(r#"{"result":"x"}"#),
        });
        let b = Bot::new("t");
        assert_eq!(b.me().unwrap_err(), "Telegram sent something odd");
        assert!(b.chats().unwrap_err().contains("webhook"));
        assert_eq!(b.check_chat("1").unwrap_err(), "Telegram error 500");
        assert!(b.send_message("1", "hi").is_ok());
    }

    #[test]
    fn group_upgraded_to_supergroup_is_followed() {
        let m = Mock::start(|r| {
            if String::from_utf8_lossy(&r.body).contains("\"-100\"") {
                (400, r#"{"ok":false,"error_code":400,"description":"Bad Request: group chat was upgraded to a supergroup chat","parameters":{"migrate_to_chat_id":-1009}}"#.into())
            } else {
                ok("{}")
            }
        });
        Bot::new("t").send_message("-100", "hi").unwrap();
        let bodies: Vec<String> = m.reqs.lock().unwrap().iter().map(|r| String::from_utf8_lossy(&r.body).into()).collect();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[1].contains("-1009"));
    }

    #[test]
    fn upload_is_proper_multipart_with_progress() {
        let m = Mock::start(|_| ok("{}"));
        let d = crate::testutil::TempDir::new("tg");
        let f = d.join("a.mp3");
        std::fs::write(&f, vec![7u8; 600_000]).unwrap();
        let mut seen = vec![];
        Bot::new("t").send_file("-5", &Upload { method: "sendAudio", field: "audio", fields: &[("title", "Take 1".into())], path: &f, filename: "Take \"1\".mp3" }, &mut |p| seen.push(p)).unwrap();
        let r = m.reqs.lock().unwrap()[0].clone();
        assert_eq!(r.path, "/bott/sendAudio");
        let b = String::from_utf8_lossy(&r.body);
        let boundary = r.headers.split("boundary=").nth(1).unwrap().lines().next().unwrap().trim().to_string();
        assert!(b.starts_with(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"chat_id\"\r\n\r\n-5\r\n")));
        assert!(b.contains("name=\"title\"\r\n\r\nTake 1\r\n"));
        assert!(b.contains("name=\"audio\"; filename=\"Take 1.mp3\"\r\nContent-Type: audio/mpeg\r\n\r\n"));
        assert!(b.ends_with(&format!("\r\n--{boundary}--\r\n")));
        assert_eq!(r.body.iter().filter(|&&x| x == 7).count(), 600_000);
        assert!(seen.len() >= 2 && *seen.last().unwrap() == 1.0, "{seen:?}");
        // WAV goes as a document
        let w = d.join("a.wav");
        std::fs::write(&w, b"RIFF").unwrap();
        let doc = |path| Upload { method: "sendDocument", field: "document", fields: &[], path, filename: "a.wav" };
        Bot::new("t").send_file("1", &doc(&w), &mut |_| {}).unwrap();
        assert!(String::from_utf8_lossy(&m.reqs.lock().unwrap()[1].body).contains("Content-Type: audio/wav"));
        assert!(Bot::new("t").send_file("1", &doc(&d.join("gone")), &mut |_| {}).unwrap_err().starts_with("Rendered file missing"));
    }

    fn tg_dest() -> Destination {
        Destination { id: "tg".into(), kind: DestKind::Telegram, label: "Band".into(), token: "123:abc".into(), chat_id: "-100".into(), chat_name: "Band Chat".into(), encoding: Encoding::Mp3, bitrate: 192, ..Default::default() }
    }

    #[test]
    fn test_and_probe() {
        let m = Mock::start(|r| match r.path.rsplit('/').next().unwrap() {
            "getMe" => ok(r#"{"username":"band_bot"}"#),
            "getChat" => ok(r#"{"title":"Band Chat"}"#),
            "sendMessage" => ok("{}"),
            _ => fail(400, "?"),
        });
        assert_eq!(test(&tg_dest()).unwrap(), "@band_bot posted to Band Chat");
        assert!(m.paths().last().unwrap().ends_with("sendMessage"));
        assert_eq!(probe(&tg_dest()).unwrap(), "Ready \u{b7} Band Chat");
        assert_eq!(test(&Destination { token: " ".into(), ..tg_dest() }).unwrap_err(), "Paste the bot token first");
        assert_eq!(test(&Destination { chat_id: "".into(), ..tg_dest() }).unwrap_err(), "Pick a chat first (FIND CHATS)");
        let _m = Mock::start(|_| fail(401, "Unauthorized"));
        assert!(test(&tg_dest()).unwrap_err().contains("token rejected"));
    }

    /// Real Telegram: `PSR_TG_TOKEN=... PSR_TG_CHAT=... cargo test live_telegram -- --ignored`
    #[test]
    #[ignore]
    fn live_telegram() {
        API_BASE.with(|b| *b.borrow_mut() = "https://api.telegram.org".into());
        let (Ok(token), Ok(chat)) = (std::env::var("PSR_TG_TOKEN"), std::env::var("PSR_TG_CHAT")) else { panic!("set PSR_TG_TOKEN and PSR_TG_CHAT") };
        let d = Destination { token, chat_id: chat, ..tg_dest() };
        println!("{:?}", Bot::new(&d.token).chats());
        println!("{}", test(&d).unwrap());
    }

    #[test]
    fn clock_format() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(75), "1:15");
        assert_eq!(clock(3725), "1:02:05");
    }
}
