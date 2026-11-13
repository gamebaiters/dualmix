//! Looks for a newer DualMix, downloads it and hands it to its own setup (port of C# `Core/Updater.cs`, scheme of
//! RP Soundboard's update checker): a `version.json` at a fixed address (or in a folder) names the latest version
//! and its package; the package is refused when its SHA-256 does not match; only https (or local folders).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use dualmix_audio::driver_setup::{self, CREATE_NO_WINDOW};
use dualmix_core::loc;
use dualmix_core::log;
use dualmix_core::updater::{DEFAULT_UPDATE_FEED, SemVer, VersionManifest};
use sha2::{Digest, Sha256};

use crate::controller::{Ev, Inbox};

const MAX_DOCUMENT_BYTES: u64 = 256 * 1024;
const MAX_PACKAGE_BYTES: u64 = 400 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Installing,
    Failed,
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub location: String,
    pub sha256: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdaterSnapshot {
    pub state: UpdateState,
    pub status_text: String,
    pub available: Option<UpdateInfo>,
    /// A check finished since the last time the controller looked (it then schedules the next one).
    pub checked: bool,
}

pub struct Updater {
    feed: String,
    st: Arc<Mutex<UpdaterSnapshot>>,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn is_web(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

fn require_https(url: &str) -> Result<String, String> {
    if url.to_ascii_lowercase().starts_with("https://") {
        Ok(url.to_string())
    } else {
        Err(format!("only https is allowed: {url}"))
    }
}

fn document_path(feed: &str) -> PathBuf {
    let p = PathBuf::from(feed);
    if p.is_dir() { p.join("version.json") } else { p }
}

/// Package address, absolute or relative to the version document.
pub fn resolve_url(url: &str, feed: &str) -> Result<String, String> {
    if is_web(url) {
        return require_https(url);
    }
    if is_web(feed) {
        let base = match feed.rfind('/') {
            Some(i) => &feed[..=i],
            None => feed,
        };
        return require_https(&format!("{base}{}", url.trim_start_matches("./")));
    }
    let p = Path::new(url);
    if p.is_absolute() {
        return Ok(url.to_string());
    }
    let dir = document_path(feed).parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    Ok(dir.join(url).to_string_lossy().into_owned())
}

fn curl_to_file(url: &str, target: &Path, max_bytes: u64, timeout_s: u32) -> Result<(), String> {
    let t = timeout_s.to_string();
    let max = max_bytes.to_string();
    let target_s = target.to_string_lossy().into_owned();
    match driver_setup::run_hidden(
        &driver_setup::system_exe("curl.exe"),
        &["-fsSL", "--proto", "=https", "--max-redirs", "5", "--max-filesize", &max, "--max-time", &t, "-A", "DualMix-Updater", "-o", &target_s, url],
        std::time::Duration::from_secs(timeout_s as u64 + 15),
    ) {
        Some((0, _)) => Ok(()),
        Some((22, _)) => Err(loc::get("Updater.Err404")),
        Some((28, _)) | None => Err(loc::get("Updater.ErrTimeout")),
        Some((code, _)) => Err(loc::fmt("Updater.ErrHttp", &[&format!("curl {code}")])),
    }
}

fn read_document(feed: &str) -> Result<String, String> {
    let bytes = if is_web(feed) {
        let url = require_https(feed)?;
        let tmp = std::env::temp_dir().join(format!("dualmix-version-{}.json", std::process::id()));
        curl_to_file(&url, &tmp, MAX_DOCUMENT_BYTES, 20)?;
        let b = std::fs::read(&tmp).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&tmp);
        b
    } else {
        let p = document_path(feed);
        let meta = std::fs::metadata(&p).map_err(|_| loc::get("Updater.ErrNotFound"))?;
        if meta.len() > MAX_DOCUMENT_BYTES {
            return Err("version document too large".into());
        }
        std::fs::read(&p).map_err(|_| loc::get("Updater.ErrNotFound"))?
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

pub fn parse_document(json: &str, feed: &str) -> Result<UpdateInfo, String> {
    let doc: VersionManifest = serde_json::from_str(json).map_err(|_| loc::get("Updater.EmptyDoc"))?;
    if !doc.product.eq_ignore_ascii_case("DualMix") {
        return Err(loc::get("Updater.WrongDoc"));
    }
    if SemVer::parse(&doc.version).is_none() {
        return Err(loc::fmt("Updater.InvalidVer", &[&doc.version]));
    }
    if doc.url.trim().is_empty() {
        return Err(loc::get("Updater.MissingUrl"));
    }
    let sha = doc.sha256.trim().to_string();
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(loc::get("Updater.MissingSha"));
    }
    Ok(UpdateInfo {
        version: doc.version.trim().to_string(),
        location: resolve_url(doc.url.trim(), feed)?,
        sha256: sha,
        notes: doc.notes.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
    })
}

fn sha256_file(p: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl Updater {
    pub fn new(settings_feed: Option<&str>) -> Self {
        let feed = std::env::var("DUALMIX_UPDATE_FEED")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| settings_feed.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
            .unwrap_or_else(|| DEFAULT_UPDATE_FEED.to_string());
        Self {
            feed,
            st: Arc::new(Mutex::new(UpdaterSnapshot { state: UpdateState::Idle, status_text: String::new(), available: None, checked: false })),
        }
    }

    pub fn set_feed(&mut self, settings_feed: Option<&str>) {
        if std::env::var("DUALMIX_UPDATE_FEED").is_err() {
            self.feed = settings_feed.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_UPDATE_FEED).to_string();
        }
    }

    pub fn snapshot(&self) -> UpdaterSnapshot {
        self.st.lock().map(|g| g.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Returns true once after every finished check (and clears the flag).
    pub fn take_checked(&self) -> bool {
        self.st.lock().map(|mut g| std::mem::take(&mut g.checked)).unwrap_or(false)
    }

    pub fn busy(&self) -> bool {
        matches!(self.snapshot().state, UpdateState::Checking | UpdateState::Downloading | UpdateState::Installing)
    }

    fn set(st: &Arc<Mutex<UpdaterSnapshot>>, inbox: &Inbox, state: UpdateState, text: String) {
        if let Ok(mut g) = st.lock() {
            g.state = state;
            g.status_text = text;
        }
        inbox.post(Ev::UpdaterChanged);
    }

    /// Reads the version document on a worker thread. The result arrives as `Ev::UpdaterChanged`.
    pub fn check(&self, inbox: Arc<Inbox>) {
        if self.busy() {
            return;
        }
        let st = Arc::clone(&self.st);
        let feed = self.feed.clone();
        Self::set(&st, &inbox, UpdateState::Checking, loc::get("Updater.Checking"));
        std::thread::spawn(move || {
            let r = read_document(&feed).and_then(|t| parse_document(&t, &feed));
            match r {
                Ok(info) => {
                    let newer = match (SemVer::parse(&info.version), SemVer::parse(current_version())) {
                        (Some(a), Some(b)) => a > b,
                        _ => false,
                    };
                    if let Ok(mut g) = st.lock() {
                        g.checked = true;
                        if newer {
                            log::info(format!("Update: version {} is available (running {})", info.version, current_version()));
                            g.state = UpdateState::Available;
                            g.status_text = loc::fmt("Updater.AvailableLine", &[&info.version]);
                            g.available = Some(info);
                        } else {
                            log::info(format!("Update: up to date ({}, latest {})", current_version(), info.version));
                            g.state = UpdateState::UpToDate;
                            g.available = None;
                            g.status_text = loc::fmt("Updater.UpToDateTime", &[&crate::controller::time_hms()[..5]]);
                        }
                    }
                    inbox.post(Ev::UpdaterChanged);
                }
                Err(e) => {
                    log::warn(format!("Update: check failed | {e}"));
                    if let Ok(mut g) = st.lock() {
                        g.checked = true;
                    }
                    Self::set(&st, &inbox, UpdateState::Failed, loc::fmt("Updater.CheckFailedPrefix", &[&e]));
                }
            }
        });
    }

    /// Downloads the package, checks it and starts its setup, which closes DualMix, replaces it and starts it again.
    pub fn install(&self, inbox: Arc<Inbox>) {
        let Some(info) = self.snapshot().available else { return };
        if self.busy() {
            return;
        }
        let st = Arc::clone(&self.st);
        Self::set(&st, &inbox, UpdateState::Downloading, loc::fmt("Updater.Downloading", &[&info.version]));
        std::thread::spawn(move || {
            let r = (|| -> Result<PathBuf, String> {
                let work = std::env::temp_dir().join("DualMix-update").join(&info.version);
                let _ = std::fs::remove_dir_all(&work);
                std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
                let zip = work.join("package.zip");
                if is_web(&info.location) {
                    curl_to_file(&require_https(&info.location)?, &zip, MAX_PACKAGE_BYTES, 900)?;
                } else {
                    std::fs::copy(&info.location, &zip).map_err(|_| loc::get("Updater.ErrNotFound"))?;
                }
                let hash = sha256_file(&zip)?;
                if !hash.eq_ignore_ascii_case(info.sha256.trim()) {
                    return Err(loc::fmt("Updater.InvalidPackage", &[&hash]));
                }
                let pkg = work.join("pkg");
                driver_setup::unzip(&zip, &pkg)?;
                let _ = std::fs::remove_file(&zip);
                let script = pkg.join("setup").join("setup.ps1");
                if !script.exists() || !pkg.join("app").join("DualMix.exe").exists() {
                    return Err(loc::get("Updater.MissingSetup"));
                }
                Ok(script)
            })();
            match r {
                Ok(script) => {
                    Self::set(&st, &inbox, UpdateState::Installing, loc::fmt("Updater.StartingInstall", &[&info.version]));
                    log::info(format!("Update: starting the setup of {} ({})", info.version, script.display()));
                    let ps = driver_setup::system_exe(r"WindowsPowerShell\v1.0\powershell.exe");
                    let conhost = driver_setup::system_exe("conhost.exe");
                    let script_s = script.to_string_lossy().into_owned();
                    let mut cmd = if Path::new(&conhost).exists() {
                        let mut c = Command::new(&conhost);
                        c.args(["--headless", &ps]);
                        c
                    } else {
                        Command::new(&ps)
                    };
                    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", &script_s, "-Update"])
                        .current_dir(std::env::temp_dir())
                        .creation_flags(CREATE_NO_WINDOW);
                    if let Err(e) = cmd.spawn() {
                        Self::set(&st, &inbox, UpdateState::Failed, loc::fmt("Updater.UpdateFailedPrefix", &[&e.to_string()]));
                        return;
                    }
                    // The setup closes this process within seconds. Still here after a while: it did not work.
                    std::thread::sleep(std::time::Duration::from_secs(180));
                    if st.lock().map(|g| g.state == UpdateState::Installing).unwrap_or(false) {
                        log::warn("Update: the setup did not replace DualMix (see setup.log)");
                        Self::set(&st, &inbox, UpdateState::Failed, loc::get("Updater.SetupStalled"));
                    }
                }
                Err(e) => {
                    log::error(format!("Update: installation failed | {e}"));
                    Self::set(&st, &inbox, UpdateState::Failed, loc::fmt("Updater.UpdateFailedPrefix", &[&e]));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_update_urls_and_reject_http() {
        assert_eq!(
            resolve_url("DualMix-2.4.0-win-x64.zip", "https://example.com/releases/version.json").unwrap(),
            "https://example.com/releases/DualMix-2.4.0-win-x64.zip"
        );
        assert!(resolve_url("http://x/y.zip", "https://example.com/version.json").is_err());
    }

    #[test]
    fn parses_manifest_with_utf8_bom() {
        let json = "\u{feff}{\"product\":\"DualMix\",\"version\":\"9.0.0\",\"url\":\"https://a/b.zip\",\"sha256\":\"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"}";
        let i = parse_document(json.trim_start_matches('\u{feff}'), "https://a/version.json").unwrap();
        assert_eq!(i.version, "9.0.0");
        assert!(parse_document("{\"product\":\"Other\",\"version\":\"1.0\",\"url\":\"x\",\"sha256\":\"00\"}", "https://a").is_err());
    }
}
