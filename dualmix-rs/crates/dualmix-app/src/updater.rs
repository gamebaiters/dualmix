//! Background version checker and verified SHA-256 self-updater (`version.json` + `setup.ps1 -Update`).

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use dualmix_core::loc;
use dualmix_core::log;
use dualmix_core::updater::{DEFAULT_UPDATE_FEED, SemVer, VersionManifest, verify_sha256_hex};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Installing,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct UpdateSnapshot {
    pub phase: UpdatePhase,
    pub available_version: Option<String>,
    pub notes: String,
    pub busy: bool,
    pub latest: Option<VersionManifest>,
    #[allow(dead_code)]
    pub postponed_version: Option<String>,
}

pub struct Updater {
    current_version: String,
    feed_url: String,
    state: Arc<Mutex<UpdateSnapshot>>,
}

impl Updater {
    pub fn new(current_version: impl Into<String>) -> Self {
        let feed_url = std::env::var("DUALMIX_UPDATE_FEED")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_UPDATE_FEED.to_string());
        Self {
            current_version: current_version.into(),
            feed_url,
            state: Arc::new(Mutex::new(UpdateSnapshot {
                phase: UpdatePhase::Idle,
                available_version: None,
                notes: String::new(),
                busy: false,
                latest: None,
                postponed_version: None,
            })),
        }
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[allow(dead_code)]
    pub fn postpone(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.postponed_version = st.available_version.clone();
    }

    pub fn ui_summary(&self) -> (String, String, String, bool) {
        let st = self.snapshot();
        match &st.phase {
            UpdatePhase::Idle | UpdatePhase::UpToDate => (
                loc::fmt("General.VersionLine", &[&self.current_version]),
                loc::get("General.UpdateIdleOn"),
                loc::get("General.UpdateCheck"),
                false,
            ),
            UpdatePhase::Checking => (
                loc::get("General.UpdateChecking"),
                loc::get("Status.Wait.Title"),
                loc::get("General.UpdateChecking"),
                true,
            ),
            UpdatePhase::Available => {
                let ver = st.available_version.as_deref().unwrap_or("");
                let body = if st.notes.trim().is_empty() {
                    loc::fmt("General.UpdateInstall", &[ver])
                } else {
                    st.notes.clone()
                };
                (
                    loc::fmt("General.UpdateInstall", &[ver]),
                    body,
                    loc::fmt("General.UpdateInstall", &[ver]),
                    false,
                )
            }
            UpdatePhase::Downloading => (
                loc::get("General.UpdateChecking"),
                loc::get("Status.Wait.Title"),
                loc::get("General.UpdateChecking"),
                true,
            ),
            UpdatePhase::Installing => (
                loc::get("Audio.Driver.Working"),
                loc::get("Status.Wait.Title"),
                loc::get("Audio.Driver.Working"),
                true,
            ),
            UpdatePhase::Error(err) => (
                loc::get("Audio.Driver.Failed"),
                err.clone(),
                loc::get("General.UpdateCheck"),
                false,
            ),
        }
    }

    pub fn check_async(&self, interactive: bool, on_change: impl Fn() + Send + 'static) {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.busy {
                return;
            }
            st.busy = true;
            st.phase = UpdatePhase::Checking;
        }
        on_change();

        let state = self.state.clone();
        let feed_url = self.feed_url.clone();
        let current = self.current_version.clone();
        std::thread::spawn(move || {
            let res = fetch_manifest(&feed_url);
            {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.busy = false;
                match res {
                    Ok(m) => {
                        let newer = match (SemVer::parse(&m.version), SemVer::parse(&current)) {
                            (Some(a), Some(b)) => a > b,
                            _ => false,
                        };
                        if newer {
                            st.available_version = Some(m.version.clone());
                            st.notes = m.notes.clone().unwrap_or_default();
                            st.latest = Some(m);
                            st.phase = UpdatePhase::Available;
                        } else {
                            st.available_version = None;
                            st.notes.clear();
                            st.latest = None;
                            st.phase = UpdatePhase::UpToDate;
                        }
                    }
                    Err(e) => {
                        log::warn(format!("Update check failed: {e}"));
                        if interactive {
                            st.phase = UpdatePhase::Error(e);
                        } else {
                            st.phase = UpdatePhase::Idle;
                        }
                    }
                }
            }
            on_change();
        });
    }

    pub fn install_async(&self, on_change: impl Fn() + Send + 'static) {
        let manifest = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.busy {
                return;
            }
            let Some(m) = st.latest.clone() else { return };
            st.busy = true;
            st.phase = UpdatePhase::Downloading;
            m
        };
        on_change();

        let state = self.state.clone();
        std::thread::spawn(move || {
            let res = download_and_launch_update(&manifest, || {
                if let Ok(mut st) = state.lock() {
                    st.phase = UpdatePhase::Installing;
                }
                on_change();
            });
            if let Err(e) = res {
                log::error(format!("Update install failed: {e}"));
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.busy = false;
                st.phase = UpdatePhase::Error(e);
                drop(st);
                on_change();
            }
        });
    }
}

fn fetch_manifest(url: &str) -> Result<VersionManifest, String> {
    let bytes = download_bytes(url)?;
    let text = String::from_utf8(bytes).map_err(|e| format!("Invalid UTF-8 manifest: {e}"))?;
    serde_json::from_str::<VersionManifest>(&text).map_err(|e| format!("Invalid JSON manifest: {e}"))
}

fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
    if let Some(local) = url.strip_prefix("file:///") {
        let path = local.replace('/', "\\");
        return std::fs::read(&path).map_err(|e| format!("Read {path}: {e}"));
    }
    if Path::new(url).exists() {
        return std::fs::read(url).map_err(|e| format!("Read {url}: {e}"));
    }
    if !url.starts_with("https://") {
        return Err("Refusing non-HTTPS update URL".to_string());
    }
    let out = Command::new("curl.exe")
        .args(["-fsSL", "--proto", "=https", "--max-time", "60", "--connect-timeout", "10", url])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("curl.exe failed: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("HTTP error: {}", err.trim()));
    }
    Ok(out.stdout)
}

fn download_and_launch_update(manifest: &VersionManifest, on_installing: impl Fn()) -> Result<(), String> {
    let stage = std::env::temp_dir().join("dualmix-update");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| format!("Stage mkdir: {e}"))?;

    let zip_path = stage.join(format!("DualMix-{}.zip", manifest.version));
    let zip_bytes = download_bytes(&manifest.url)?;
    if !manifest.sha256.trim().is_empty() && !verify_sha256_hex(&zip_bytes, &manifest.sha256) {
        return Err("SHA-256 verification failed".to_string());
    }
    std::fs::write(&zip_path, &zip_bytes).map_err(|e| format!("Write zip: {e}"))?;

    let extract_dir = stage.join("extracted");
    std::fs::create_dir_all(&extract_dir).map_err(|e| format!("Extract mkdir: {e}"))?;

    let tar = Command::new("tar.exe")
        .args(["-xf", &zip_path.to_string_lossy(), "-C", &extract_dir.to_string_lossy()])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("tar.exe failed: {e}"))?;
    if !tar.status.success() {
        let err = String::from_utf8_lossy(&tar.stderr);
        return Err(format!("Archive extraction failed: {}", err.trim()));
    }

    on_installing();

    let source_root = locate_payload_root(&extract_dir);
    let setup_ps1 = source_root.join("setup").join("setup.ps1");
    if !setup_ps1.exists() {
        return Err("Update archive missing setup/setup.ps1".to_string());
    }
    let app_dir = if source_root.join("app").join("DualMix.exe").exists() {
        source_root.join("app")
    } else {
        source_root.clone()
    };

    let cmd = format!(
        "& '{}' -Update -Source '{}'",
        setup_ps1.to_string_lossy().replace('\'', "''"),
        app_dir.to_string_lossy().replace('\'', "''")
    );
    Command::new("conhost.exe")
        .args(["--headless", "powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &cmd])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("Launch updater failed: {e}"))?;

    log::info(format!("Update {} staged; launched setup.ps1 -Update", manifest.version));
    Ok(())
}

fn locate_payload_root(extract_dir: &Path) -> PathBuf {
    if extract_dir.join("setup").join("setup.ps1").exists() || extract_dir.join("DualMix.exe").exists() {
        return extract_dir.to_path_buf();
    }
    if let Ok(entries) = std::fs::read_dir(extract_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && (p.join("setup").join("setup.ps1").exists() || p.join("DualMix.exe").exists()) {
                return p;
            }
        }
    }
    extract_dir.to_path_buf()
}
