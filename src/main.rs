//! ra_fetch_gui - RetroAchievements sets for MiaUI, downloaded on the PC.
//!
//! The same three actions as the console's own RetroAchievements.pak (ra_download.py): download the sets that
//! are missing, do that for one system, or refresh everything. The games can be read from:
//!   - the connected console, through the MiaUI Web Manager over USB networking (/api/list, and /api/hash,
//!     which runs minarch's own rcheevos hasher on the console), or
//!   - a local folder (a copy of Roms/ or the SD card), hashed here by hash.rs, a port checked against the
//!     console's hasher file for file.
//! The sets go to the console (/api/upload) or to a local folder, in the layout minarch reads:
//!     Profile/retroachievements/data/<TAG>/<hash>.json   the raw `r=patch` response
//!     Profile/retroachievements/progress/<TAG>/          created empty, minarch fills it in
//!     Profile/retroachievements/index.json               what was downloaded, for NekoBit

mod hash;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use directories::ProjectDirs;
use eframe::egui;
use egui::{Color32, CornerRadius, Margin, RichText, Stroke};

use hash::{ARCADE_TAG_LIST, DISC_EXTS, IGNORED_EXTS, Method, console_for, ext_of, hash_rom, method_for_file};

const DOREQUEST_URL: &str = "https://retroachievements.org/dorequest.php";
/// dorequest.php only answers emulators, and wants to be talked to by something that looks like one.
const USER_AGENT: &str = "RetroArch/1.16.0";
/// Minimum spacing between RetroAchievements calls. Be a good guest.
const DELAY: Duration = Duration::from_millis(500);
/// The console's address on the USB network (usbnet.sh).
const DEFAULT_IP: &str = "192.168.137.100";
/// Where minarch reads the sets, relative to /userdata (and to the root of a local output folder).
const RA_DIR: &str = "Profile/retroachievements";

// ===========================================================================
// RetroAchievements client
// ===========================================================================

struct RaClient {
    username: String,
    http: reqwest::blocking::Client,
    token: Option<String>,
    last_call: Option<Instant>,
}

impl RaClient {
    fn new(username: &str) -> Result<Self> {
        Ok(RaClient {
            username: username.to_string(),
            http: reqwest::blocking::Client::builder()
                .user_agent(USER_AGENT)
                .build()?,
            token: None,
            last_call: None,
        })
    }

    /// Never more than one request per DELAY.
    fn throttle(&mut self) {
        if let Some(last) = self.last_call {
            let elapsed = last.elapsed();
            if elapsed < DELAY {
                std::thread::sleep(DELAY - elapsed);
            }
        }
        self.last_call = Some(Instant::now());
    }

    fn post(&mut self, params: &[(&str, String)], timeout: Duration) -> Result<(String, serde_json::Value)> {
        self.throttle();
        let reply = self
            .http
            .post(DOREQUEST_URL)
            .form(params)
            .timeout(timeout)
            .send()?
            .error_for_status()?;
        let text = reply.text()?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
            let head: String = text.chars().take(120).collect();
            anyhow!("server sent back non-JSON: {head:?}")
        })?;
        Ok((text, parsed))
    }

    fn login(&mut self, password: &str) -> Result<()> {
        let u = self.username.clone();
        let (_, data) = self.post(
            &[
                ("r", "login".into()),
                ("u", u),
                ("p", password.to_string()),
            ],
            Duration::from_secs(30),
        )?;
        if data.get("Success").and_then(|v| v.as_bool()) != Some(true) {
            let msg = data
                .get("Error")
                .and_then(|v| v.as_str())
                .unwrap_or("login failed");
            bail!("{msg}");
        }
        self.token = data
            .get("Token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(())
    }

    /// Reuse a cached token. Returns false if it's no longer good.
    fn login_with_token(&mut self, token: &str) -> Result<bool> {
        self.token = Some(token.to_string());
        let u = self.username.clone();
        let (_, data) = self.post(
            &[
                ("r", "login".into()),
                ("u", u),
                ("t", token.to_string()),
            ],
            Duration::from_secs(30),
        )?;
        Ok(data.get("Success").and_then(|v| v.as_bool()) == Some(true))
    }

    fn game_id(&mut self, md5: &str) -> Result<u64> {
        let u = self.username.clone();
        let t = self.token.clone().unwrap_or_default();
        let (_, data) = self.post(
            &[
                ("r", "gameid".into()),
                ("u", u),
                ("t", t),
                ("m", md5.to_string()),
            ],
            Duration::from_secs(30),
        )?;
        Ok(match data.get("GameID") {
            Some(v) if v.is_u64() => v.as_u64().unwrap_or(0),
            Some(v) => v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
            None => 0,
        })
    }

    /// Returns the raw response text (what minarch parses) and the parsed json.
    fn patch(&mut self, game_id: u64) -> Result<(String, serde_json::Value)> {
        let u = self.username.clone();
        let t = self.token.clone().unwrap_or_default();
        self.post(
            &[
                ("r", "patch".into()),
                ("u", u),
                ("t", t),
                ("g", game_id.to_string()),
            ],
            Duration::from_secs(60),
        )
    }
}

/// Where our own state lives. Deliberately not under the output folder: that
/// gets copied to the SD card. `directories` resolves this per platform -
/// ~/.cache/ra_fetch on Linux (which is also where the Python script put its
/// token, so an existing session carries over), ~/Library/Caches/ra_fetch on
/// macOS and %LOCALAPPDATA%\ra_fetch\cache on Windows.
fn app_cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "ra_fetch").map(|d| d.cache_dir().to_path_buf())
}

fn token_cache_path() -> Option<PathBuf> {
    Some(app_cache_dir()?.join("token.json"))
}



fn load_cached_token(username: &str) -> Option<String> {
    let path = token_cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v.get("user").and_then(|u| u.as_str()) == Some(username) {
        v.get("token").and_then(|t| t.as_str()).map(String::from)
    } else {
        None
    }
}

fn save_cached_token(username: &str, token: &str) {
    let Some(path) = token_cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = serde_json::json!({ "user": username, "token": token });
    if std::fs::write(&path, body.to_string()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}


// ===========================================================================
// the console: MiaUI Web Manager's API
// ===========================================================================

/// Percent-encoding for a query value.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Clone)]
struct Console {
    base: String,
    http: reqwest::blocking::Client,
}

struct RemoteEntry {
    name: String,
    dir: bool,
    size: u64,
    mtime: i64,
}

impl Console {
    fn new(ip: &str) -> Self {
        let ip = ip.trim().trim_start_matches("http://").trim_end_matches('/');
        let ip = if ip.is_empty() { DEFAULT_IP } else { ip };
        Console {
            base: format!("http://{ip}"),
            http: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(300)) // a big disc image takes a while to hash
                .build()
                .expect("http client"),
        }
    }

    /// The server's own error message for a failed call, so the log can show it.
    fn check(reply: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
        if reply.status().is_success() {
            return Ok(reply);
        }
        let status = reply.status();
        let msg = reply
            .json::<serde_json::Value>()
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| status.to_string());
        Err(anyhow!("{msg}"))
    }

    fn list(&self, rel: &str) -> Result<Vec<RemoteEntry>> {
        let reply = Self::check(self.http.get(format!("{}/api/list?path={}", self.base, enc(rel))).send()?)?;
        let v: serde_json::Value = reply.json()?;
        Ok(v["entries"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| RemoteEntry {
                        name: e["name"].as_str().unwrap_or("").to_string(),
                        dir: e["dir"].as_bool().unwrap_or(false),
                        size: e["size"].as_u64().unwrap_or(0),
                        mtime: e["mtime"].as_i64().unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The hash minarch computes for a game on the console: Ok(Ok(hash)), Ok(Err(why)) when it can't be hashed,
    /// Err when the console didn't answer.
    fn hash(&self, rel: &str) -> Result<std::result::Result<String, String>> {
        let reply = self.http.get(format!("{}/api/hash?path={}", self.base, enc(rel))).send()?;
        if reply.status() == reqwest::StatusCode::NOT_FOUND || reply.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            bail!("the console's Web Manager is too old to hash games: update it");
        }
        match Self::check(reply) {
            Ok(r) => Ok(Ok(r.json::<serde_json::Value>()?["hash"].as_str().unwrap_or("").to_string())),
            Err(e) => Ok(Err(e.to_string())),
        }
    }

    /// A file's bytes, or None when there's no such file.
    fn download(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        let reply = self.http.get(format!("{}/api/download?path={}", self.base, enc(rel))).send()?;
        if reply.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::check(reply)?.bytes()?.to_vec()))
    }

    fn upload(&self, rel: &str, body: Vec<u8>) -> Result<()> {
        Self::check(self.http.put(format!("{}/api/upload?path={}", self.base, enc(rel))).body(body).send()?)?;
        Ok(())
    }

    /// Creates a folder; one that already exists is fine.
    fn mkdir(&self, rel: &str) -> Result<()> {
        let reply = self.http.post(format!("{}/api/mkdir?path={}", self.base, enc(rel))).send()?;
        if reply.status() == reqwest::StatusCode::CONFLICT {
            return Ok(());
        }
        Self::check(reply)?;
        Ok(())
    }

    /// "Connected · 20.7 GB free", or why not.
    fn probe(&self) -> std::result::Result<String, String> {
        let quick = reqwest::blocking::Client::builder().timeout(Duration::from_secs(4)).build().map_err(|e| e.to_string())?;
        let reply = quick
            .get(format!("{}/api/list?path=", self.base))
            .send()
            .map_err(|_| "Not reachable. Is the cable plugged in, with USB networking on?".to_string())?;
        if !reply.status().is_success() {
            return Err(format!("Something answered, but not the MiaUI Web Manager ({})", reply.status()));
        }
        let v: serde_json::Value = reply.json().map_err(|_| "Something answered, but not the MiaUI Web Manager".to_string())?;
        // /api/hash answers a bad request when it exists, 404 on a Web Manager from before it
        let has_hash = quick
            .get(format!("{}/api/hash?path=", self.base))
            .send()
            .map(|r| r.status() != reqwest::StatusCode::NOT_FOUND)
            .unwrap_or(false);
        if !has_hash {
            return Err("Connected, but its Web Manager is too old to hash games: update it".into());
        }
        Ok(format!("Connected · {} free", size_text(v["free"].as_u64().unwrap_or(0))))
    }
}

fn size_text(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", units[i]) }
}

// ===========================================================================
// games: what to hash, found the same way on a console and in a folder
// ===========================================================================

/// "Nintendo Game Boy (GB)" -> "GB", as minarch's getEmuName() reads it.
fn folder_tag(folder: &str) -> Option<String> {
    let folder = folder.trim_end();
    let open = folder.rfind('(')?;
    folder.ends_with(')').then(|| folder[open + 1..folder.len() - 1].trim().to_string()).filter(|t| !t.is_empty())
}

/// One entry of "Choose a system": a Roms folder, or every arcade folder together (one RetroAchievements
/// system, split by core).
#[derive(Clone)]
struct System {
    label: String,
    folders: Vec<String>,
}

const ARCADE_LABEL: &str = "Arcade (MAME / FBNeo)";

fn systems_of(mut folders: Vec<String>) -> Vec<System> {
    folders.retain(|f| !f.starts_with('.') && folder_tag(f).is_some_and(|t| console_for(&t).is_some()));
    let is_arcade = |f: &String| folder_tag(f).is_some_and(|t| ARCADE_TAG_LIST.contains(&t.as_str()));
    let arcade: Vec<String> = folders.iter().filter(|f| is_arcade(f)).cloned().collect();
    let mut out: Vec<System> = folders.into_iter().filter(|f| !is_arcade(f)).map(|f| System { label: f.clone(), folders: vec![f] }).collect();
    if !arcade.is_empty() {
        out.push(System { label: ARCADE_LABEL.into(), folders: arcade });
    }
    out.sort_by_key(|s| s.label.to_lowercase());
    out
}

/// A file found under a Roms folder, before deciding whether it's a game.
struct Found {
    /// relative to the Roms folder, with '/'
    rel: String,
    size: u64,
    mtime: i64,
}

struct Game {
    tag: String,
    folder: String,
    method: Method,
    console_id: u32,
    rel: String,
    size: u64,
    mtime: i64,
}

impl Game {
    fn name(&self) -> &str {
        self.rel.rsplit('/').next().unwrap_or(&self.rel)
    }
}

/// The FILE entries of a cue sheet: its tracks, which aren't games of their own.
fn cue_files(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if !l.get(..4).is_some_and(|w| w.eq_ignore_ascii_case("FILE")) {
                return None;
            }
            let rest = l[4..].trim();
            Some(match rest.strip_prefix('"') {
                Some(q) => q.split('"').next().unwrap_or("").to_string(),
                None => rest.split_whitespace().next().unwrap_or("").to_string(),
            })
        })
        .filter(|n| !n.is_empty())
        .collect()
}

/// The games among one folder's files, as ra_download.py picks them: a .cue owns its tracks, disc systems only
/// take disc images, cartridge systems never do (except PC Engine CD games in the PCE folder).
/// `owned` holds the lower-cased relative paths of the files cue sheets refer to.
fn games_of(folder: &str, files: Vec<Found>, owned: &HashSet<String>) -> Vec<Game> {
    let Some(tag) = folder_tag(folder) else { return Vec::new() };
    let Some((console_id, method)) = console_for(&tag) else { return Vec::new() };
    let mut out = Vec::new();
    for f in files {
        let path = Path::new(&f.rel);
        let ext = ext_of(path);
        let name = f.rel.rsplit('/').next().unwrap_or("");
        if name.starts_with('.') || IGNORED_EXTS.contains(&ext.as_str()) || ext == ".7z" || owned.contains(&f.rel.to_lowercase()) {
            continue;
        }
        let disc_system = matches!(method, Method::Psx | Method::SegaCd);
        if disc_system && !DISC_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let pce_cd = method_for_file(method, path) == Method::PceCd;
        if !disc_system && method != Method::Arcade && !pce_cd && matches!(ext.as_str(), ".cue" | ".iso" | ".chd") {
            continue;
        }
        out.push(Game { tag: tag.clone(), folder: folder.to_string(), method, console_id, rel: f.rel, size: f.size, mtime: f.mtime });
    }
    out
}

fn cue_owned(cue_rel: &str, text: &str, owned: &mut HashSet<String>) {
    let dir = cue_rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    for f in cue_files(text) {
        owned.insert(format!("{dir}/{f}").trim_start_matches('/').to_lowercase());
    }
}

// ===========================================================================
// where the games come from, and where the sets go
// ===========================================================================

#[derive(Clone)]
enum Source {
    Console(Console),
    Local(PathBuf),
}

impl Source {
    fn describe(&self) -> String {
        match self {
            Source::Console(c) => format!("the console at {}", c.base.trim_start_matches("http://")),
            Source::Local(p) => p.display().to_string(),
        }
    }

    /// The cache key prefix: a game is remembered per source.
    fn id(&self) -> String {
        match self {
            Source::Console(c) => format!("{}/userdata/Roms/", c.base),
            Source::Local(p) => format!("{}/", p.display()),
        }
    }

    fn folders(&self) -> Result<Vec<String>> {
        Ok(match self {
            Source::Console(c) => c.list("Roms")?.into_iter().filter(|e| e.dir).map(|e| e.name).collect(),
            Source::Local(p) => {
                if !p.is_dir() {
                    bail!("no such folder: {}", p.display());
                }
                std::fs::read_dir(p)?
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            }
        })
    }

    fn games(&self, folders: &[String], cancel: &AtomicBool, say: &dyn Fn(String)) -> Result<Vec<Game>> {
        let mut out = Vec::new();
        for folder in folders {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            say(format!("looking in {folder}"));
            let (mut files, mut owned) = (Vec::new(), HashSet::new());
            match self {
                Source::Console(c) => {
                    let mut stack = vec![folder.clone()];
                    while let Some(dir) = stack.pop() {
                        for e in c.list(&format!("Roms/{dir}"))? {
                            if e.name.starts_with('.') {
                                continue;
                            }
                            let rel = format!("{dir}/{}", e.name);
                            if e.dir {
                                stack.push(rel);
                            } else {
                                if e.name.to_lowercase().ends_with(".cue") && e.size < 64 * 1024 {
                                    if let Some(text) = c.download(&format!("Roms/{rel}"))? {
                                        cue_owned(&rel, &String::from_utf8_lossy(&text), &mut owned);
                                    }
                                }
                                files.push(Found { rel, size: e.size, mtime: e.mtime });
                            }
                        }
                    }
                }
                Source::Local(root) => {
                    let mut stack = vec![root.join(folder)];
                    while let Some(dir) = stack.pop() {
                        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                        for e in rd.flatten() {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if name.starts_with('.') {
                                continue;
                            }
                            let path = e.path();
                            let Ok(md) = std::fs::metadata(&path) else { continue };
                            if md.is_dir() {
                                stack.push(path);
                                continue;
                            }
                            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                            if name.to_lowercase().ends_with(".cue") {
                                if let Ok(text) = std::fs::read(&path) {
                                    cue_owned(&rel, &String::from_utf8_lossy(&text), &mut owned);
                                }
                            }
                            let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
                            files.push(Found { rel, size: md.len(), mtime });
                        }
                    }
                }
            }
            files.sort_by(|a, b| a.rel.cmp(&b.rel));
            out.extend(games_of(folder, files, &owned));
        }
        Ok(out)
    }

    /// Ok(Ok(hash)), Ok(Err(why)) for a game that can't be hashed, Err when the console stopped answering.
    fn hash(&self, g: &Game) -> Result<std::result::Result<String, String>> {
        match self {
            Source::Console(c) => c.hash(&format!("Roms/{}", g.rel)),
            Source::Local(root) => Ok(match hash_rom(&g.tag, g.method, &root.join(&g.rel)) {
                Ok(Some(h)) => Ok(h),
                Ok(None) => Err(format!("not a {} game this can hash", g.tag)),
                Err(e) => Err(e.to_string()),
            }),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct ManifestEntry {
    hash: String,
    game_id: u64,
    title: String,
    tag: String,
    console_id: u32,
    achievements: u64,
    rom: String,
}

enum Output {
    Console(Console),
    /// <folder>/Profile/retroachievements
    Local(PathBuf),
}

struct Target {
    out: Output,
    /// the sets already there, by tag (read once per tag)
    have: HashMap<String, HashSet<String>>,
}

impl Target {
    fn describe(&self) -> String {
        match &self.out {
            Output::Console(c) => format!("the console at {} ({})", c.base.trim_start_matches("http://"), "/userdata/".to_string() + RA_DIR),
            Output::Local(p) => p.display().to_string(),
        }
    }

    fn sets_in(&mut self, tag: &str) -> Result<&HashSet<String>> {
        if !self.have.contains_key(tag) {
            let names: Vec<String> = match &self.out {
                Output::Console(c) => match c.list(&format!("{RA_DIR}/data/{tag}")) {
                    Ok(v) => v.into_iter().map(|e| e.name).collect(),
                    Err(_) => Vec::new(), // no folder yet
                },
                Output::Local(p) => std::fs::read_dir(p.join("data").join(tag))
                    .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                    .unwrap_or_default(),
            };
            let set = names.into_iter().filter_map(|n| n.strip_suffix(".json").map(|h| h.to_lowercase())).collect();
            self.have.insert(tag.to_string(), set);
        }
        Ok(&self.have[tag])
    }

    /// Whether minarch will find a set for this game: an arcade set filed under any arcade tag counts.
    fn has(&mut self, tag: &str, hash: &str) -> Result<bool> {
        let tags: Vec<&str> = if ARCADE_TAG_LIST.contains(&tag) { ARCADE_TAG_LIST.to_vec() } else { vec![tag] };
        for t in tags {
            if self.sets_in(t)?.contains(hash) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The `r=patch` response, verbatim: minarch feeds it to rc_api_process_fetch_game_data_response().
    fn write(&mut self, tag: &str, hash: &str, raw: &str) -> Result<()> {
        match &self.out {
            Output::Console(c) => {
                c.upload(&format!("{RA_DIR}/data/{tag}/{hash}.json"), raw.as_bytes().to_vec())?;
                c.mkdir(&format!("{RA_DIR}/progress"))?;
                c.mkdir(&format!("{RA_DIR}/progress/{tag}"))?;
            }
            Output::Local(p) => {
                let target = p.join("data").join(tag).join(format!("{hash}.json"));
                std::fs::create_dir_all(target.parent().unwrap())?;
                std::fs::create_dir_all(p.join("progress").join(tag))?;
                let tmp = target.with_extension("json.tmp");
                std::fs::write(&tmp, raw.as_bytes())?;
                std::fs::rename(&tmp, &target)?;
            }
        }
        self.sets_in(tag)?;
        self.have.get_mut(tag).unwrap().insert(hash.to_string());
        Ok(())
    }

    /// index.json lists every downloaded set (NekoBit reads it); new entries are merged into what's there.
    fn merge_index(&self, new: &[ManifestEntry]) -> Result<()> {
        if new.is_empty() {
            return Ok(());
        }
        let old: Option<Vec<u8>> = match &self.out {
            Output::Console(c) => c.download(&format!("{RA_DIR}/index.json"))?,
            Output::Local(p) => std::fs::read(p.join("index.json")).ok(),
        };
        let mut merged: BTreeMap<String, ManifestEntry> = BTreeMap::new();
        if let Some(Ok(existing)) = old.map(|b| serde_json::from_slice::<Vec<ManifestEntry>>(&b)) {
            merged.extend(existing.into_iter().map(|e| (e.hash.clone(), e)));
        }
        merged.extend(new.iter().map(|e| (e.hash.clone(), e.clone())));
        let mut all: Vec<ManifestEntry> = merged.into_values().collect();
        all.sort_by(|a, b| a.title.cmp(&b.title));
        let body = serde_json::to_vec_pretty(&all)?;
        match &self.out {
            Output::Console(c) => c.upload(&format!("{RA_DIR}/index.json"), body)?,
            Output::Local(p) => {
                std::fs::create_dir_all(p)?;
                std::fs::write(p.join("index.json"), body)?;
            }
        }
        Ok(())
    }
}

// ===========================================================================
// what is remembered between runs (as the console app's ra_hashes.json)
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct Cache {
    /// game (source id + path) -> [size, mtime, hash]: an unchanged game isn't hashed again
    files: HashMap<String, (u64, i64, String)>,
    /// hash -> "none" (no such game) / "noach" (no achievements): not asked again by "Download new"
    neg: HashMap<String, String>,
}

impl Cache {
    fn path() -> Option<PathBuf> {
        Some(app_cache_dir()?.join("hashes.json"))
    }
    fn load() -> Self {
        Self::path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        if let Some(p) = Self::path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            if let Ok(b) = serde_json::to_vec(self) {
                let _ = std::fs::write(p, b);
            }
        }
    }
}

// ===========================================================================
// the job, on a worker thread (egui must never wait on the network or a disc image)
// ===========================================================================

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Head,
    Ok,
    Skip,
    Miss,
    Err,
    Info,
}

struct Line {
    kind: Kind,
    text: String,
}

#[derive(Clone, Copy, Default)]
struct Counts {
    new: usize,
    have: usize,
    none: usize,
    bad: usize,
    failed: usize,
}

#[derive(Clone, Default)]
struct Progress {
    done: usize,
    total: usize,
    system: String,
    game: String,
}

enum Msg {
    Line(Line),
    Progress(Progress),
    Counts(Counts),
    Finished,
}

struct JobCfg {
    source: Source,
    output: Output,
    /// None: every system the source has
    folders: Option<Vec<String>>,
    /// "Complete update": ask again about every game and replace the sets already there
    full: bool,
    username: String,
}

struct Reporter {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Reporter {
    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        self.ctx.request_repaint();
    }
    fn say(&self, kind: Kind, text: impl Into<String>) {
        self.send(Msg::Line(Line { kind, text: text.into() }));
    }
}

fn run_job(cfg: JobCfg, rep: Reporter, cancel: Arc<AtomicBool>) {
    let mut cache = Cache::load();
    if let Err(e) = job(&cfg, &rep, &cancel, &mut cache) {
        rep.say(Kind::Err, format!("Stopped: {e}"));
    }
    cache.save();
    rep.send(Msg::Finished);
}

fn job(cfg: &JobCfg, rep: &Reporter, cancel: &AtomicBool, cache: &mut Cache) -> Result<()> {
    let mut target = Target { out: match &cfg.output {
        Output::Console(c) => Output::Console(c.clone()),
        Output::Local(p) => Output::Local(p.clone()),
    }, have: HashMap::new() };
    rep.say(Kind::Head, format!("Games from {}", cfg.source.describe()));
    rep.say(Kind::Head, format!("Achievements to {}", target.describe()));

    // the session saved by Settings > Log in
    let token = load_cached_token(&cfg.username).ok_or_else(|| anyhow!("log in to RetroAchievements first (⚙ Settings)"))?;
    let mut client = RaClient::new(&cfg.username)?;
    if !client.login_with_token(&token).map_err(|e| anyhow!("can't reach RetroAchievements: {e}"))? {
        bail!("the saved RetroAchievements session has expired: log in again (⚙ Settings)");
    }

    let systems = systems_of(cfg.source.folders()?);
    let folders: Vec<String> = match &cfg.folders {
        Some(f) => f.clone(),
        None => systems.iter().flat_map(|s| s.folders.clone()).collect(),
    };
    if folders.is_empty() {
        bail!("no system folders with achievements support were found in {}", cfg.source.describe());
    }
    let games = cfg.source.games(&folders, cancel, &|m| rep.send(Msg::Progress(Progress { system: m, ..Default::default() })))?;
    let n_systems = games.iter().map(|g| &g.folder).collect::<HashSet<_>>().len();
    rep.say(Kind::Info, format!("{} games in {} systems", games.len(), n_systems));
    if cfg.full {
        rep.say(Kind::Info, "Complete update: every game is checked again and its set replaced");
    }

    let id = cfg.source.id();
    let mut counts = Counts::default();
    let mut seen = HashSet::new();
    let mut manifest = Vec::new();
    let total = games.len();
    let mut stopped = false;
    for (i, g) in games.iter().enumerate() {
        rep.send(Msg::Progress(Progress { done: i, total, system: g.tag.clone(), game: g.name().to_string() }));
        rep.send(Msg::Counts(counts));
        if cancel.load(Ordering::Relaxed) {
            stopped = true;
            break;
        }
        let label = format!("[{}] {}", g.tag, g.name());
        let key = format!("{id}{}", g.rel);

        let digest = match cache.files.get(&key).filter(|c| c.0 == g.size && c.1 == g.mtime) {
            Some(c) => c.2.clone(),
            None => match cfg.source.hash(g)? {
                Ok(h) => {
                    cache.files.insert(key, (g.size, g.mtime, h.clone()));
                    h
                }
                Err(why) => {
                    rep.say(Kind::Skip, format!("  can't hash   {label}: {why}"));
                    counts.bad += 1;
                    continue;
                }
            },
        };
        if !seen.insert(digest.clone()) {
            continue; // the same game twice (a zip and its unzipped copy)
        }
        if !cfg.full && target.has(&g.tag, &digest)? {
            counts.have += 1;
            continue;
        }
        if !cfg.full && cache.neg.contains_key(&digest) {
            counts.none += 1;
            continue;
        }

        let game_id = match client.game_id(&digest) {
            Ok(id) => id,
            Err(e) => {
                rep.say(Kind::Err, format!("  failed       {label}: {e}"));
                counts.failed += 1;
                continue;
            }
        };
        if game_id == 0 {
            rep.say(Kind::Miss, format!("  unknown      {label}: not in the RetroAchievements database"));
            cache.neg.insert(digest, "none".into());
            counts.none += 1;
            continue;
        }
        let (raw, parsed) = match client.patch(game_id) {
            Ok(v) => v,
            Err(e) => {
                rep.say(Kind::Err, format!("  failed       {label}: {e}"));
                counts.failed += 1;
                continue;
            }
        };
        let patch = &parsed["PatchData"];
        let achievements = patch["Achievements"].as_array().cloned().unwrap_or_default();
        if parsed["Success"].as_bool() != Some(true) || achievements.is_empty() {
            rep.say(Kind::Miss, format!("  no set       {label}: game {game_id} has no achievements yet"));
            cache.neg.insert(digest, "noach".into());
            counts.none += 1;
            continue;
        }
        if let Err(e) = target.write(&g.tag, &digest, &raw) {
            rep.say(Kind::Err, format!("  failed       {label}: can't save the set: {e}"));
            counts.failed += 1;
            continue;
        }
        cache.neg.remove(&digest);
        let title = patch["Title"].as_str().map(String::from).unwrap_or_else(|| format!("game {game_id}"));
        // the "Warning: Outdated Emulator" stub is filtered out on the console too
        let count = achievements.iter().filter(|a| a["ID"].as_u64().unwrap_or(0) < 101_000_000).count() as u64;
        rep.say(Kind::Ok, format!("  downloaded   {label}  →  {title} ({count} achievements)"));
        manifest.push(ManifestEntry { hash: digest, game_id, title, tag: g.tag.clone(), console_id: g.console_id, achievements: count, rom: g.name().to_string() });
        counts.new += 1;
    }
    if !stopped {
        rep.send(Msg::Progress(Progress { done: total, total, ..Default::default() }));
    }
    rep.send(Msg::Counts(counts));
    target.merge_index(&manifest)?;

    rep.say(
        Kind::Head,
        format!(
            "{}{} new, {} already there, {} without a set, {} couldn't be hashed, {} failed",
            if stopped { "Stopped. " } else { "Done. " },
            counts.new, counts.have, counts.none, counts.bad, counts.failed
        ),
    );
    if counts.new > 0 {
        if let Output::Local(p) = &cfg.output {
            let profile = p.parent().unwrap_or(p);
            rep.say(Kind::Ok, format!("Copy {} to the root of the console's SD card, merging it with the Profile folder there.", profile.display()));
        } else {
            rep.say(Kind::Ok, "The sets are on the console: the games show their achievements next time they start.");
        }
    }
    Ok(())
}

// ===========================================================================
// settings (eframe stores them in the clear: never the password)
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Default)]
enum Mode {
    #[default]
    Console,
    Local,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
#[serde(default)]
struct Settings {
    source: Mode,
    output: Mode,
    /// the local Roms folder (the one holding "Nintendo Game Boy (GB)" and the like)
    roms: String,
    /// the local output folder: the sets go to <out>/Profile/retroachievements
    out: String,
    ip: String,
    username: String,
}

impl Settings {
    fn ip(&self) -> String {
        if self.ip.trim().is_empty() { DEFAULT_IP.into() } else { self.ip.trim().into() }
    }
    fn out_dir(&self) -> PathBuf {
        Path::new(self.out.trim()).join("Profile").join("retroachievements")
    }
}

// ===========================================================================
// the window
// ===========================================================================

mod theme {
    use super::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x1b, 0x1f, 0x26);
    pub const BAR: Color32 = Color32::from_rgb(0x20, 0x24, 0x2c);
    pub const CARD: Color32 = Color32::from_rgb(0x24, 0x29, 0x32);
    pub const FIELD: Color32 = Color32::from_rgb(0x16, 0x19, 0x1f);
    pub const BUTTON: Color32 = Color32::from_rgb(0x2e, 0x34, 0x3f);
    pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0x37, 0x3e, 0x4b);
    pub const LINE: Color32 = Color32::from_rgb(0x33, 0x3a, 0x46);
    pub const TEXT: Color32 = Color32::from_rgb(0xe9, 0xec, 0xf1);
    pub const MUTED: Color32 = Color32::from_rgb(0x9a, 0xa3, 0xb0);
    pub const ACCENT: Color32 = Color32::from_rgb(0x5a, 0xa7, 0xff);
    pub const PRIMARY: Color32 = Color32::from_rgb(0x2b, 0x62, 0xa8);
    pub const OK: Color32 = Color32::from_rgb(0x46, 0xd1, 0x90);
    pub const WARN: Color32 = Color32::from_rgb(0xf2, 0xb8, 0x4b);
    pub const BAD: Color32 = Color32::from_rgb(0xff, 0x6e, 0x6e);
    pub const SELECTED: Color32 = Color32::from_rgb(0xdb, 0xde, 0xe5); // light selection, black text
}

/// A flat dark theme: no shadows, one radius, light selection with black text.
fn setup_style(ctx: &egui::Context) {
    use theme::*;
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    v.window_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = BAR;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = SELECTED;
    v.selection.stroke = Stroke::new(1.0, Color32::BLACK);
    v.override_text_color = None;
    let r = CornerRadius::same(6);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.noninteractive.corner_radius = r;
    for (w, fill, stroke) in [
        (&mut v.widgets.inactive, BUTTON, LINE),
        (&mut v.widgets.hovered, BUTTON_HOVER, ACCENT),
        (&mut v.widgets.active, BUTTON_HOVER, ACCENT),
        (&mut v.widgets.open, BUTTON_HOVER, LINE),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::new(1.0, stroke);
        w.fg_stroke = Stroke::new(1.0, TEXT);
        w.corner_radius = r;
        w.expansion = 0.0;
    }
    // always dark, whatever the desktop's own theme
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        use egui::{FontFamily, FontId, TextStyle};
        s.text_styles = [
            (TextStyle::Heading, FontId::new(21.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(10.0, 10.0);
        s.spacing.button_padding = egui::vec2(14.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.window_margin = Margin::same(20);
        s.spacing.text_edit_width = 340.0;
    });
}

enum Bg {
    Conn(std::result::Result<String, String>),
    Login(std::result::Result<String, String>),
    Systems(std::result::Result<Vec<System>, String>),
}

#[derive(Clone, PartialEq)]
enum Status {
    Unknown,
    Busy,
    Good(String),
    Bad(String),
}

enum Picker {
    Closed,
    Loading,
    Ready(Vec<System>),
    Failed(String),
}

const MAX_LOG_LINES: usize = 20_000;

struct App {
    cfg: Settings,
    password: String,
    settings_open: bool,
    conn: Status,
    conn_for: String,
    login: Status,
    bg_tx: Sender<Bg>,
    bg_rx: Receiver<Bg>,
    picker: Picker,

    log: Vec<Line>,
    counts: Counts,
    progress: Progress,
    running: bool,
    job_rx: Option<Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
}

impl App {
    fn new(cfg: Settings, ctx: &egui::Context) -> Self {
        let (bg_tx, bg_rx) = channel();
        let login = match load_cached_token(cfg.username.trim()) {
            Some(_) if !cfg.username.trim().is_empty() => Status::Good(format!("Logged in as {}", cfg.username.trim())),
            _ => Status::Unknown,
        };
        let mut app = App {
            settings_open: login == Status::Unknown, // first run: the account is the first thing to fill in
            cfg,
            password: String::new(),
            conn: Status::Unknown,
            conn_for: String::new(),
            login,
            bg_tx,
            bg_rx,
            picker: Picker::Closed,
            log: Vec::new(),
            counts: Counts::default(),
            progress: Progress::default(),
            running: false,
            job_rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if app.uses_console() {
            app.test_connection(ctx);
        }
        app
    }

    fn uses_console(&self) -> bool {
        self.cfg.source == Mode::Console || self.cfg.output == Mode::Console
    }

    fn test_connection(&mut self, ctx: &egui::Context) {
        self.conn = Status::Busy;
        self.conn_for = self.cfg.ip();
        let (tx, ctx, console) = (self.bg_tx.clone(), ctx.clone(), Console::new(&self.cfg.ip()));
        std::thread::spawn(move || {
            let _ = tx.send(Bg::Conn(console.probe()));
            ctx.request_repaint();
        });
    }

    fn log_in(&mut self, ctx: &egui::Context) {
        let (user, password) = (self.cfg.username.trim().to_string(), std::mem::take(&mut self.password));
        if user.is_empty() || password.is_empty() {
            self.login = Status::Bad("Type your RetroAchievements user name and password".into());
            return;
        }
        self.login = Status::Busy;
        let (tx, ctx) = (self.bg_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = RaClient::new(&user).and_then(|mut c| {
                c.login(&password)?;
                let token = c.token.clone().ok_or_else(|| anyhow!("the server sent no session"))?;
                save_cached_token(&user, &token);
                Ok(format!("Logged in as {user}"))
            });
            let _ = tx.send(Bg::Login(result.map_err(|e| format!("Login failed: {e}"))));
            ctx.request_repaint();
        });
    }

    fn log_out(&mut self) {
        if let Some(p) = token_cache_path() {
            let _ = std::fs::remove_file(p);
        }
        self.login = Status::Unknown;
    }

    fn source(&self) -> Source {
        match self.cfg.source {
            Mode::Console => Source::Console(Console::new(&self.cfg.ip())),
            Mode::Local => Source::Local(PathBuf::from(self.cfg.roms.trim())),
        }
    }

    /// Why the actions can't run yet, in the user's words; None when everything is in place.
    fn blocker(&self) -> Option<String> {
        if !matches!(self.login, Status::Good(_)) {
            return Some("Log in to RetroAchievements first: ⚙ Settings, top right.".into());
        }
        if self.cfg.source == Mode::Local && !Path::new(self.cfg.roms.trim()).is_dir() {
            return Some("Choose the folder with your games (SOURCE).".into());
        }
        if self.cfg.output == Mode::Local && self.cfg.out.trim().is_empty() {
            return Some("Choose where to save the achievements (OUTPUT).".into());
        }
        if self.uses_console() && !matches!(self.conn, Status::Good(_)) {
            return Some(match &self.conn {
                Status::Busy => "Connecting to the console…".into(),
                _ => "Connect the console (USB networking on), then press Test.".into(),
            });
        }
        None
    }

    fn start(&mut self, ctx: &egui::Context, folders: Option<Vec<String>>, full: bool) {
        self.log.clear();
        self.counts = Counts::default();
        self.progress = Progress::default();
        self.cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        self.job_rx = Some(rx);
        self.running = true;
        let cfg = JobCfg {
            source: self.source(),
            output: match self.cfg.output {
                Mode::Console => Output::Console(Console::new(&self.cfg.ip())),
                Mode::Local => Output::Local(self.cfg.out_dir()),
            },
            folders,
            full,
            username: self.cfg.username.trim().to_string(),
        };
        let (rep, cancel) = (Reporter { tx, ctx: ctx.clone() }, Arc::clone(&self.cancel));
        std::thread::spawn(move || run_job(cfg, rep, cancel));
    }

    fn open_picker(&mut self, ctx: &egui::Context) {
        self.picker = Picker::Loading;
        let (tx, ctx, source) = (self.bg_tx.clone(), ctx.clone(), self.source());
        std::thread::spawn(move || {
            let r = source.folders().map(systems_of).map_err(|e| e.to_string());
            let _ = tx.send(Bg::Systems(r));
            ctx.request_repaint();
        });
    }

    fn drain(&mut self) {
        while let Ok(m) = self.bg_rx.try_recv() {
            match m {
                Bg::Conn(r) => self.conn = r.map_or_else(Status::Bad, Status::Good),
                Bg::Login(r) => self.login = r.map_or_else(Status::Bad, Status::Good),
                Bg::Systems(r) => {
                    if !matches!(self.picker, Picker::Closed) {
                        self.picker = match r {
                            Ok(s) if s.is_empty() => Picker::Failed("No system with achievements support was found.".into()),
                            Ok(s) => Picker::Ready(s),
                            Err(e) => Picker::Failed(e),
                        };
                    }
                }
            }
        }
        let mut finished = false;
        if let Some(rx) = &self.job_rx {
            for m in rx.try_iter() {
                match m {
                    Msg::Line(l) => {
                        if self.log.len() >= MAX_LOG_LINES {
                            self.log.drain(..MAX_LOG_LINES / 4);
                        }
                        self.log.push(l);
                    }
                    Msg::Progress(p) => self.progress = p,
                    Msg::Counts(c) => self.counts = c,
                    Msg::Finished => finished = true,
                }
            }
        }
        if finished {
            self.running = false;
            self.job_rx = None;
        }
    }
}

fn status_line(ui: &mut egui::Ui, s: &Status, idle: &str) {
    let (color, text) = match s {
        Status::Unknown => (theme::MUTED, idle.to_string()),
        Status::Busy => (theme::WARN, "Checking…".to_string()),
        Status::Good(t) => (theme::OK, t.clone()),
        Status::Bad(t) => (theme::BAD, t.clone()),
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 5.0, color);
        ui.label(RichText::new(text).color(if matches!(s, Status::Unknown) { theme::MUTED } else { theme::TEXT }));
    });
}

fn card<R>(ui: &mut egui::Ui, step: &str, title: &str, hint: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::LINE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(step).strong().color(theme::ACCENT));
                ui.label(RichText::new(title).strong().size(14.0).color(theme::TEXT));
                ui.label(RichText::new(hint).color(theme::MUTED));
            });
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

/// The two-way choice at the top of SOURCE and OUTPUT.
fn mode_switch(ui: &mut egui::Ui, mode: &mut Mode, enabled: bool, labels: [&str; 2]) -> bool {
    let before = *mode;
    ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (m, label) in [(Mode::Console, labels[0]), (Mode::Local, labels[1])] {
                let selected = *mode == m;
                let text = RichText::new(label).color(if selected { Color32::BLACK } else { theme::TEXT });
                let button = egui::Button::new(text)
                    .fill(if selected { theme::SELECTED } else { theme::BUTTON })
                    .stroke(Stroke::new(1.0, if selected { theme::SELECTED } else { theme::LINE }))
                    .min_size(egui::vec2(170.0, 34.0));
                if ui.add(button).clicked() {
                    *mode = m;
                }
            }
        });
    });
    *mode != before
}

/// A one-line text field as tall as the buttons beside it.
fn field(text: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).margin(Margin::symmetric(8, 6))
}

fn folder_field(ui: &mut egui::Ui, value: &mut String, enabled: bool, hint: &str) {
    ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 110.0).max(160.0);
            ui.add(field(value).hint_text(hint).desired_width(w));
            if ui.button("Browse…").clicked() {
                let mut d = rfd::FileDialog::new();
                if !value.trim().is_empty() && Path::new(value.trim()).is_dir() {
                    d = d.set_directory(value.trim());
                }
                if let Some(p) = d.pick_folder() {
                    *value = p.display().to_string();
                }
            }
        });
    });
}

/// A big action button: a bold title over a short explanation.
fn action_button(ui: &mut egui::Ui, enabled: bool, title: &str, sub: &str, width: f32) -> bool {
    let mut job = egui::text::LayoutJob::default();
    job.append(title, 0.0, egui::TextFormat { font_id: egui::FontId::proportional(15.5), color: theme::TEXT, ..Default::default() });
    job.append(&format!("\n{sub}"), 0.0, egui::TextFormat { font_id: egui::FontId::proportional(12.5), color: theme::MUTED, ..Default::default() });
    ui.add_enabled(enabled, egui::Button::new(job).min_size(egui::vec2(width, 58.0))).clicked()
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.cfg);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain();
        let ctx = ui.ctx().clone();
        let idle = !self.running;

        // ---- title bar --------------------------------------------------------------------------------
        egui::Panel::top("title")
            .frame(egui::Frame::new().fill(theme::BAR).inner_margin(Margin::symmetric(24, 14)).stroke(Stroke::new(1.0, theme::LINE)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("RetroAchievements").heading().strong().color(theme::TEXT));
                    ui.label(RichText::new("for MiaUI").heading().color(theme::ACCENT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let open = self.settings_open;
                        let gear = egui::Button::new(RichText::new("⚙  Settings").color(if open { Color32::BLACK } else { theme::TEXT }))
                            .fill(if open { theme::SELECTED } else { theme::BUTTON });
                        if ui.add(gear).on_hover_text("Console address and RetroAchievements account").clicked() {
                            self.settings_open = !self.settings_open;
                        }
                        match &self.login {
                            Status::Good(t) => ui.label(RichText::new(t).color(theme::MUTED)),
                            _ => ui.label(RichText::new("Not logged in").color(theme::WARN)),
                        };
                    });
                });
            });

        // ---- settings (collapsible) -----------------------------------------------------------------------
        if self.settings_open {
            egui::Panel::top("settings")
                .frame(egui::Frame::new().fill(theme::BAR).inner_margin(Margin { left: 24, right: 24, top: 6, bottom: 18 }).stroke(Stroke::new(1.0, theme::LINE)))
                .show(ui, |ui| {
                    ui.columns(2, |cols| {
                        card(&mut cols[0], "", "CONSOLE CONNECTION", "", |ui| {
                            ui.label(RichText::new("The console's address on the USB network").color(theme::MUTED));
                            ui.horizontal(|ui| {
                                ui.add_enabled(idle, field(&mut self.cfg.ip).hint_text(DEFAULT_IP).desired_width(170.0));
                                if ui.add_enabled(idle && self.conn != Status::Busy, egui::Button::new("Test connection")).clicked() {
                                    self.test_connection(&ctx);
                                }
                            });
                            status_line(ui, &self.conn, "Not tested yet");
                        });
                        card(&mut cols[1], "", "RETROACHIEVEMENTS ACCOUNT", "", |ui| {
                            if let Status::Good(t) = &self.login {
                                let t = t.clone();
                                status_line(ui, &Status::Good(t), "");
                                ui.label(RichText::new("Only the session is kept on this PC, never your password.").color(theme::MUTED));
                                if ui.add_enabled(idle, egui::Button::new("Log out")).clicked() {
                                    self.log_out();
                                }
                            } else {
                                egui::Grid::new("account").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                                    ui.label("User name");
                                    ui.add(field(&mut self.cfg.username).desired_width(220.0));
                                    ui.end_row();
                                    ui.label("Password");
                                    let r = ui.add(field(&mut self.password).password(true).desired_width(220.0));
                                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                        self.log_in(&ctx);
                                    }
                                    ui.end_row();
                                });
                                ui.horizontal(|ui| {
                                    if ui.add_enabled(self.login != Status::Busy, egui::Button::new("Log in")).clicked() {
                                        self.log_in(&ctx);
                                    }
                                    status_line(ui, &self.login, "Not logged in");
                                });
                            }
                        });
                    });
                });
        }

        // ---- progress (bottom) -----------------------------------------------------------------------------
        egui::Panel::bottom("progress")
            .frame(egui::Frame::new().fill(theme::BAR).inner_margin(Margin::symmetric(24, 12)).stroke(Stroke::new(1.0, theme::LINE)))
            .show(ui, |ui| {
                let p = &self.progress;
                let fraction = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
                let text = if p.total > 0 {
                    let current = if p.done < p.total { format!("  ·  {}  ·  {}", p.system, p.game) } else { String::new() };
                    format!("Game {} / {}{current}", (p.done + usize::from(p.done < p.total)).min(p.total), p.total)
                } else if self.running {
                    if p.system.is_empty() { "Starting…".into() } else { format!("Finding games: {}", p.system) }
                } else {
                    "Ready".into()
                };
                // nothing to show yet: an empty track, not a dot of fill
                let fill = if fraction > 0.0 { theme::PRIMARY } else { theme::FIELD };
                ui.add(egui::ProgressBar::new(fraction).text(RichText::new(text).color(theme::TEXT)).desired_height(22.0).fill(fill));
                let c = self.counts;
                ui.horizontal(|ui| {
                    for (n, what, color) in [
                        (c.new, "new", theme::OK),
                        (c.have, "already there", theme::MUTED),
                        (c.none, "without a set", theme::WARN),
                        (c.bad, "couldn't be hashed", theme::MUTED),
                        (c.failed, "failed", theme::BAD),
                    ] {
                        ui.label(RichText::new(n.to_string()).strong().color(if n > 0 { color } else { theme::MUTED }));
                        ui.label(RichText::new(what).color(theme::MUTED));
                        ui.add_space(10.0);
                    }
                });
            });

        // ---- main column -----------------------------------------------------------------------------------
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::symmetric(24, 20))).show(ui, |ui| {
            let mut retest = false;
            ui.columns(2, |cols| {
                card(&mut cols[0], "1", "SOURCE", "Where are your games?", |ui| {
                    retest |= mode_switch(ui, &mut self.cfg.source, idle, ["Connected console", "Local folder / SD card"]);
                    ui.add_space(2.0);
                    match self.cfg.source {
                        Mode::Console => {
                            ui.label(RichText::new(format!("Games in /userdata/Roms on the console at {}", self.cfg.ip())).color(theme::MUTED));
                            status_line(ui, &self.conn, "Not tested yet");
                        }
                        Mode::Local => {
                            ui.label(RichText::new("The Roms folder, the one holding folders like \"Nintendo Game Boy (GB)\"").color(theme::MUTED));
                            folder_field(ui, &mut self.cfg.roms, idle, "e.g. E:\\Roms");
                        }
                    }
                });
                card(&mut cols[1], "2", "OUTPUT", "Where do the achievements go?", |ui| {
                    retest |= mode_switch(ui, &mut self.cfg.output, idle, ["Connected console", "Local folder"]);
                    ui.add_space(2.0);
                    match self.cfg.output {
                        Mode::Console => {
                            ui.label(RichText::new(format!("Straight onto the console, in /userdata/{RA_DIR}")).color(theme::MUTED));
                            status_line(ui, &self.conn, "Not tested yet");
                        }
                        Mode::Local => {
                            ui.label(RichText::new("A folder on this PC, or the SD card's root").color(theme::MUTED));
                            folder_field(ui, &mut self.cfg.out, idle, "e.g. E:\\");
                            if !self.cfg.out.trim().is_empty() {
                                ui.label(RichText::new(format!("Saved in {}", self.cfg.out_dir().display())).small().color(theme::MUTED));
                            }
                        }
                    }
                });
            });
            if retest && self.uses_console() && self.conn != Status::Busy && !matches!(self.conn, Status::Good(_)) {
                self.test_connection(&ctx);
            }
            if self.conn_for != self.cfg.ip() && !matches!(self.conn, Status::Busy) && matches!(self.conn, Status::Good(_)) {
                self.conn = Status::Unknown; // the address changed: test it again
            }

            ui.add_space(8.0);
            let blocker = self.blocker();
            let ready = idle && blocker.is_none();
            card(ui, "3", "ACTION", "What should it do?", |ui| {
                ui.horizontal(|ui| {
                    let w = ((ui.available_width() - 3.0 * 10.0 - 120.0) / 3.0).max(150.0);
                    if action_button(ui, ready, "Download new achievements", "Only games that don't have them yet", w) {
                        self.start(&ctx, None, false);
                    }
                    if action_button(ui, ready, "Choose a system…", "The same, for one system", w) {
                        self.open_picker(&ctx);
                    }
                    if action_button(ui, ready, "Complete update", "Check every game again, replace all sets", w) {
                        self.start(&ctx, None, true);
                    }
                    let stop = egui::Button::new(RichText::new("■  Stop").strong().color(if self.running { theme::BAD } else { theme::MUTED }))
                        .min_size(egui::vec2(110.0, 58.0))
                        .stroke(Stroke::new(1.0, if self.running { theme::BAD } else { theme::LINE }));
                    if ui.add_enabled(self.running, stop).clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                });
                if let Some(b) = blocker.filter(|_| idle) {
                    ui.label(RichText::new(format!("⚠  {b}")).color(theme::WARN));
                }
            });

            ui.add_space(8.0);
            egui::Frame::new()
                .fill(theme::FIELD)
                .stroke(Stroke::new(1.0, theme::LINE))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(14))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(ui.available_height());
                    if self.log.is_empty() {
                        ui.label(RichText::new("What happens shows up here: pick where your games are and where the achievements go, then choose an action.").color(theme::MUTED));
                    }
                    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show_rows(
                        ui,
                        ui.text_style_height(&egui::TextStyle::Monospace),
                        self.log.len(),
                        |ui, rows| {
                            for line in &self.log[rows] {
                                ui.label(RichText::new(&line.text).monospace().color(colour_for(line.kind)));
                            }
                        },
                    );
                });
        });

        // ---- "Choose a system" ---------------------------------------------------------------------------
        if !matches!(self.picker, Picker::Closed) {
            let mut chosen: Option<System> = None;
            let mut close = false;
            let frame = egui::Frame::new().fill(theme::CARD).stroke(Stroke::new(1.0, theme::LINE)).inner_margin(Margin::same(20)).corner_radius(CornerRadius::same(10));
            let modal = egui::Modal::new(egui::Id::new("systems")).frame(frame).show(&ctx, |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("Choose a system").heading().strong());
                ui.label(RichText::new("Only this system's games are checked.").color(theme::MUTED));
                ui.add_space(6.0);
                match &self.picker {
                    Picker::Loading => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Reading the systems…");
                        });
                    }
                    Picker::Failed(e) => {
                        ui.label(RichText::new(e).color(theme::BAD));
                    }
                    Picker::Ready(list) => {
                        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                            for s in list {
                                if ui.add(egui::Button::new(&s.label).min_size(egui::vec2(ui.available_width(), 34.0))).clicked() {
                                    chosen = Some(s.clone());
                                }
                            }
                        });
                    }
                    Picker::Closed => {}
                }
                ui.add_space(6.0);
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
            if modal.should_close() {
                close = true;
            }
            if let Some(s) = chosen {
                self.picker = Picker::Closed;
                self.start(&ctx, Some(s.folders), false);
            } else if close {
                self.picker = Picker::Closed;
            }
        }

        if self.running || self.conn == Status::Busy || self.login == Status::Busy || matches!(self.picker, Picker::Loading) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

fn colour_for(kind: Kind) -> Color32 {
    match kind {
        Kind::Head => theme::ACCENT,
        Kind::Ok => theme::OK,
        Kind::Skip => theme::MUTED,
        Kind::Miss => theme::WARN,
        Kind::Err => theme::BAD,
        Kind::Info => theme::TEXT,
    }
}

fn main() -> eframe::Result {
    // `ra_fetch_gui --hash TAG FILE`: the hash this app computes for one game (support and testing)
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "--hash" {
        match console_for(&args[2]) {
            Some((_, method)) => match hash_rom(&args[2], method, Path::new(&args[3])) {
                Ok(Some(h)) => println!("{h}"),
                Ok(None) => println!("-"),
                Err(e) => println!("error: {e}"),
            },
            None => println!("unknown tag {}", args[2]),
        }
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 820.0])
            .with_min_inner_size([900.0, 640.0])
            .with_title("RetroAchievements for MiaUI"),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "ra_fetch",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            let cfg = cc.storage.and_then(|s| eframe::get_value::<Settings>(s, eframe::APP_KEY)).unwrap_or_default();
            Ok(Box::new(App::new(cfg, &cc.egui_ctx)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(rel: &str) -> Found {
        Found { rel: rel.into(), size: 1, mtime: 1 }
    }

    #[test]
    fn games_are_picked_as_on_the_console() {
        // a cue sheet owns its tracks; a PCE folder takes cartridges and PC Engine CD discs
        let mut owned = HashSet::new();
        cue_owned("PC Engine (PCE)/Gate/Gate.cue", "FILE \"Gate (Track 01).bin\" BINARY\nFILE \"Gate (Track 02).bin\" BINARY\n", &mut owned);
        let files = ["PC Engine (PCE)/Bonk.pce", "PC Engine (PCE)/Gate/Gate.cue", "PC Engine (PCE)/Gate/Gate (Track 01).bin",
            "PC Engine (PCE)/Gate/Gate (Track 02).bin", "PC Engine (PCE)/Ys.chd", "PC Engine (PCE)/notes.txt"];
        let games = games_of("PC Engine (PCE)", files.iter().map(|f| found(f)).collect(), &owned);
        let names: Vec<&str> = games.iter().map(|g| g.name()).collect();
        assert_eq!(names, ["Bonk.pce", "Gate.cue", "Ys.chd"]);
        assert_eq!(method_for_file(games[1].method, Path::new(&games[1].rel)), Method::PceCd);
        // a disc image in a cartridge system isn't a game; PS takes only discs
        let games = games_of("Nintendo Game Boy (GB)", vec![found("x/a.gb"), found("x/b.iso")], &HashSet::new());
        assert_eq!(games.len(), 1);
        let games = games_of("Sony PlayStation (PS)", vec![found("p/a.chd"), found("p/b.gb")], &HashSet::new());
        assert_eq!(games.len(), 1);
    }

    #[test]
    fn arcade_folders_are_one_system() {
        let s = systems_of(vec!["Arcade (MAME2003P)".into(), "Neo Geo (FBNEOGEO)".into(), "Nintendo NES (FC)".into(), "Tools".into(), "MSDos (DOSBOX)".into()]);
        let labels: Vec<&str> = s.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, [ARCADE_LABEL, "Nintendo NES (FC)"]);
        assert_eq!(s[0].folders.len(), 2);
    }

    /// A real run against RetroAchievements with the session saved by the app, printed as the log would show it:
    ///   RA_SOURCE=console:IP | local:DIR  RA_OUTPUT=console:IP | local:DIR  [RA_FOLDERS="a,b"] [RA_FULL=1]
    ///   cargo test --release live_run -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_run() {
        let user = std::fs::read(token_cache_path().unwrap()).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["user"].as_str().map(String::from)).expect("log in with the app first");
        let pick = |v: &str| -> (bool, String) {
            let s = std::env::var(v).unwrap_or_default();
            let (k, rest) = s.split_once(':').expect("console:IP or local:DIR");
            (k == "console", rest.to_string())
        };
        let (sc, s) = pick("RA_SOURCE");
        let (oc, o) = pick("RA_OUTPUT");
        let cfg = JobCfg {
            source: if sc { Source::Console(Console::new(&s)) } else { Source::Local(s.into()) },
            output: if oc { Output::Console(Console::new(&o)) } else { Output::Local(o.into()) },
            folders: std::env::var("RA_FOLDERS").ok().map(|f| f.split(',').map(String::from).collect()),
            full: std::env::var("RA_FULL").is_ok(),
            username: user,
        };
        let (tx, rx) = channel();
        run_job(cfg, Reporter { tx, ctx: egui::Context::default() }, Arc::new(AtomicBool::new(false)));
        for m in rx.try_iter() {
            match m {
                Msg::Line(l) => println!("{}", l.text),
                Msg::Counts(c) => println!("   counts: {} new {} have {} none {} bad {} failed", c.new, c.have, c.none, c.bad, c.failed),
                _ => {}
            }
        }
    }

    #[test]
    fn tags_and_encoding() {
        assert_eq!(folder_tag("Nintendo Game Boy (GB)").as_deref(), Some("GB"));
        assert_eq!(folder_tag("Tools"), None);
        assert_eq!(enc("Roms/Game Boy (GB)/a&b.gb"), "Roms/Game%20Boy%20%28GB%29/a%26b.gb");
    }
}

/// Offscreen renders of the window (no display needed): `cargo test --release ui_shots -- --ignored`, PNGs in
/// $UI_SHOTS (default target/ui_shots).
#[cfg(test)]
mod ui_shots {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn shot(name: &str, cfg: Settings, settings_open: bool, demo_log: bool) {
        shot_with(name, cfg, settings_open, demo_log, false)
    }

    fn shot_with(name: &str, cfg: Settings, settings_open: bool, demo_log: bool, picker: bool) {
        let mut harness = Harness::builder().with_size(egui::vec2(1120.0, 820.0)).build_eframe(|cc| {
            setup_style(&cc.egui_ctx);
            let mut app = App::new(cfg.clone(), &cc.egui_ctx);
            app.settings_open = settings_open;
            if picker {
                app.login = Status::Good("Logged in as DSkywalk".into());
                app.picker = Picker::Ready(systems_of(["Arcade (MAME2003P)", "Neo Geo (FBNEOGEO)", "Atari Lynx (HANDY)", "Nintendo Game Boy (GB)",
                    "Nintendo SNES (SFC)", "PC Engine CD (PCE)", "Sega Megadrive (MD)", "Sony PlayStation (PS)"].map(String::from).to_vec()));
            }
            if demo_log {
                app.login = Status::Good("Logged in as DSkywalk".into());
                app.conn = Status::Good("Connected · 20.7 GB free".into());
                app.running = true;
                app.progress = Progress { done: 41, total: 487, system: "PCE".into(), game: "Lords of Thunder.chd".into() };
                app.counts = Counts { new: 12, have: 25, none: 4, bad: 1, failed: 0 };
                for (k, t) in [
                    (Kind::Head, "Games from the console at 192.168.137.100"),
                    (Kind::Head, "Achievements to the console at 192.168.137.100 (/userdata/Profile/retroachievements)"),
                    (Kind::Info, "487 games in 12 systems"),
                    (Kind::Ok, "  downloaded   [PCE] Gradius II - Gofer no Yabou.chd  →  Gradius II: Gofer no Yabou (41 achievements)"),
                    (Kind::Miss, "  no set       [PCE] Gain Ground SX.chd: game 13195 has no achievements yet"),
                    (Kind::Skip, "  can't hash   [PCE] Ys 1 And 2.chd: Could not open track"),
                    (Kind::Miss, "  unknown      [PCE] Red Alert.chd: not in the RetroAchievements database"),
                    (Kind::Ok, "  downloaded   [PCE] Lords of Thunder.chd  →  Lords of Thunder (59 achievements)"),
                ] {
                    app.log.push(Line { kind: k, text: t.into() });
                }
            }
            app
        });
        harness.run_steps(4);
        let dir = std::env::var("UI_SHOTS").unwrap_or_else(|_| "target/ui_shots".into());
        std::fs::create_dir_all(&dir).unwrap();
        harness.render().unwrap().save(format!("{dir}/{name}.png")).unwrap();
        let _ = harness.query_by_label("Download new achievements");
    }

    #[test]
    #[ignore]
    fn ui_shots() {
        let mut cfg = Settings { username: "DSkywalk".into(), ..Default::default() };
        shot("1_console_running", cfg.clone(), false, true);
        cfg.source = Mode::Local;
        cfg.output = Mode::Local;
        cfg.roms = "/media/sd/Roms".into();
        cfg.out = "/media/sd".into();
        shot("2_local_settings_open", cfg.clone(), true, false);
        shot_with("3_choose_system", cfg.clone(), false, false, true);
    }
}
