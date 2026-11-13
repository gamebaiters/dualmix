//! Virtual-cable driver management and endpoint configuration, port of C# `DualMix.Audio.DriverSetup`.
//!
//! DualMix uses VB-Audio cables: their playback endpoints become "DualMix Game" / "DualMix Chat". The free package is
//! always downloaded from the official site and its Authenticode signatures are verified before the official setup
//! runs. Also hosts the "repair Windows audio" action and the elevated `--setup` entry point.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dualmix_core::loc;
use dualmix_core::log;
use dualmix_core::settings::Settings;
use serde::{Deserialize, Serialize};

use crate::devices::{self, DeviceInfo, Flow, Role, STATE_ACTIVE, STATE_DISABLED, STATE_UNPLUGGED};
use crate::format::{self, AudioFormat};

pub const PACKAGE_URL: &str = "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip";
pub const HOME_PAGE: &str = "https://vb-audio.com/Cable/";
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SetupResult {
    pub pack_rejected: bool,
    pub ok: bool,
    pub declined: bool,
    pub installed: bool,
    pub repaired: bool,
    pub reboot_recommended: bool,
    pub message: String,
}

/// At least one usable virtual cable (enough for the Chat output).
pub fn cables_present() -> bool {
    devices::list(Flow::Render, STATE_ACTIVE | STATE_DISABLED).iter().any(DeviceInfo::is_vb_cable)
}

pub fn driver_installed() -> bool {
    devices::list(Flow::Render, STATE_ACTIVE | STATE_DISABLED | STATE_UNPLUGGED).iter().any(DeviceInfo::is_vb_cable)
}

/// Runs a program without a console window and returns (exit code, stdout).
pub fn run_hidden(exe: &str, args: &[&str], timeout: Duration) -> Option<(i32, String)> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut s);
        String::from_utf8_lossy(&s).into_owned()
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some((st.code().unwrap_or(-1), reader.join().unwrap_or_default())),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

pub fn system_exe(name: &str) -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let p = Path::new(&root).join("System32").join(name);
    p.to_string_lossy().into_owned()
}

pub fn powershell(script: &str, timeout: Duration) -> Option<(i32, String)> {
    let ps = system_exe(r"WindowsPowerShell\v1.0\powershell.exe");
    run_hidden(&ps, &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script], timeout)
}

/// Downloads an https resource with the Windows built-in curl.exe.
pub fn download(url: &str, target: &Path, timeout_s: u32) -> Result<(), String> {
    let t = timeout_s.to_string();
    let target_s = target.to_string_lossy().into_owned();
    match run_hidden(
        &system_exe("curl.exe"),
        &["-fsSL", "--proto", "=https", "--max-redirs", "5", "--max-time", &t, "-A", "DualMix", "-o", &target_s, url],
        Duration::from_secs(timeout_s as u64 + 10),
    ) {
        Some((0, _)) => Ok(()),
        Some((code, _)) => Err(format!("download failed (curl exit {code})")),
        None => Err("download timed out".into()),
    }
}

/// Extracts a zip with the Windows built-in tar.exe (bsdtar).
pub fn unzip(zip: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let (z, d) = (zip.to_string_lossy().into_owned(), dir.to_string_lossy().into_owned());
    match run_hidden(&system_exe("tar.exe"), &["-xf", &z, "-C", &d], Duration::from_secs(300)) {
        Some((0, _)) => Ok(()),
        _ => Err(format!("cannot extract {}", zip.display())),
    }
}

/// Returns the signer subject of a validly Authenticode-signed file, or None.
pub fn verify_signature(path: &Path) -> Option<String> {
    let p = path.to_string_lossy().replace('\'', "''");
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{p}'; if ($s.Status -eq 'Valid') {{ $s.SignerCertificate.Subject }} else {{ 'INVALID:' + $s.Status }}"
    );
    let (_, out) = powershell(&script, Duration::from_secs(60))?;
    let out = out.trim().to_string();
    if out.is_empty() || out.starts_with("INVALID:") {
        log::warn(format!("Signature check failed for {} ({out})", path.display()));
        None
    } else {
        Some(out)
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let n = (crate::router::tick_ms() as u64) ^ (std::process::id() as u64) << 20;
    let d = std::env::temp_dir().join(format!("{prefix}-{n:08x}"));
    let _ = std::fs::create_dir_all(&d);
    d
}

fn wait_for(cond: impl Fn() -> bool, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    cond()
}

fn find_files(dir: &Path, name_pred: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                find_files(&p, name_pred, out);
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(name_pred) {
                out.push(p);
            }
        }
    }
}

// ================================================================== elevated side

/// Entry point of `DualMix.exe --setup <result> [--install] [--repair] [--pack <zip>] [--unconfigure] [--remove-drivers]`.
pub fn run_elevated(args: &[String]) -> i32 {
    let has = |f: &str| args.iter().any(|a| a.eq_ignore_ascii_case(f));
    let value_of = |f: &str| args.iter().position(|a| a.eq_ignore_ascii_case(f)).and_then(|i| args.get(i + 1)).cloned();
    let result_file = value_of("--setup").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("dualmix-setup.json"));
    let (install, repair, unconfigure, remove_drivers) = (has("--install"), has("--repair"), has("--unconfigure"), has("--remove-drivers"));
    let pack = value_of("--pack");
    let mut result = SetupResult::default();
    let mut messages: Vec<String> = Vec::new();
    log::info(format!("Setup (elevated) started: install={install}, repair={repair}"));

    let body = || -> Result<(), String> {
        if unconfigure || remove_drivers {
            if remove_drivers {
                let n = remove_cables(true);
                messages.push(loc::fmt("Setup.CablesRemoved", &[&n.to_string()]));
            } else if unconfigure_endpoints() {
                messages.push(loc::get("Setup.CablesRestored"));
            }
            return Ok(());
        }
        if repair {
            repair_audio_services();
            result.repaired = true;
            messages.push(loc::get("Setup.AudioRepaired"));
        }
        if install && !cables_present() {
            let before = DefaultsSnapshot::take();
            install_vb_cable(&mut messages)?;
            result.installed = true;
            if !wait_for(cables_present, Duration::from_secs(40)) {
                // The driver is in place but Windows has not created the endpoints yet.
                repair_audio_services();
                if !wait_for(cables_present, Duration::from_secs(20)) {
                    result.reboot_recommended = true;
                    messages.push(loc::get("Setup.RebootNeeded"));
                }
            }
            // Windows likes to make a freshly installed device the default one: put everything back.
            std::thread::sleep(Duration::from_millis(1500));
            before.restore();
        }
        if let Some(p) = &pack {
            install_cable_pack(Path::new(p), &mut messages, &mut result)?;
        }
        if cables_present() {
            let s = Settings::load_file(&dualmix_core::log::settings_file());
            configure(&s, true, Some(&mut messages));
            // A second pass picks up the state Windows publishes after renames / format changes.
            std::thread::sleep(Duration::from_millis(500));
            configure(&s, true, None);
        } else if install && !result.reboot_recommended {
            result.reboot_recommended = true;
            messages.push(loc::get("Setup.RebootPending"));
        }
        Ok(())
    };
    let mut body = body;
    match body() {
        Ok(()) => result.ok = true,
        Err(e) => {
            log::error(format!("Elevated setup failed | {e}"));
            messages.push(e);
            result.ok = false;
        }
    }
    result.message = messages.join(" ");
    if let Err(e) = std::fs::write(&result_file, serde_json::to_string(&result).unwrap_or_default()) {
        log::error(format!("Cannot write setup result | {e}"));
    }
    log::info(format!("Setup (elevated) finished: {}", result.message));
    if result.ok { 0 } else { 1 }
}

fn install_vb_cable(messages: &mut Vec<String>) -> Result<(), String> {
    let work = temp_dir("DualMix-driver");
    let r = (|| {
        let zip = work.join("vbcable.zip");
        log::info(format!("Downloading {PACKAGE_URL}"));
        download(PACKAGE_URL, &zip, 180)?;
        let dir = work.join("pkg");
        unzip(&zip, &dir)?;
        let setup = dir.join("VBCABLE_Setup_x64.exe");
        if !setup.exists() {
            return Err("VB-CABLE package incomplete (setup missing).".into());
        }
        // Never run something we did not verify: valid signature from VB-Audio's author, catalog signed by Microsoft.
        let signer = verify_signature(&setup).ok_or_else(|| loc::get("Setup.VbSigInvalid"))?;
        if !signer.to_uppercase().contains("BUREL") {
            return Err(loc::fmt("Setup.VbUnexpectedSigner", &[&signer]));
        }
        let cat = dir.join("vbaudio_cable64_win10.cat");
        if cat.exists() {
            match verify_signature(&cat) {
                Some(s) if s.to_lowercase().contains("microsoft") => {}
                _ => return Err(loc::get("Setup.VbCatInvalid")),
            }
        }
        log::info(format!("VB-CABLE package verified, signer: {signer}"));
        let code = run_setup_program(&setup, &["-i", "-h"])?;
        log::info(format!("VB-CABLE setup exit code {code}"));
        messages.push(loc::get("Setup.VbInstalled"));
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    r
}

fn run_setup_program(setup: &Path, args: &[&str]) -> Result<i32, String> {
    let dir = setup.parent().unwrap_or(Path::new("."));
    let mut child = Command::new(setup)
        .args(args)
        .current_dir(dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|_| loc::fmt("Setup.CantLaunchSetup", &[&setup.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()]))?;
    let end = Instant::now() + Duration::from_secs(240);
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            return Ok(st.code().unwrap_or(-1));
        }
        if Instant::now() > end {
            let _ = child.kill();
            return Err(loc::get("Setup.DriverTimeout"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Folder where a purchased cable pack is kept, so it can be reinstalled automatically.
pub fn pack_cache_dir() -> PathBuf {
    dualmix_core::log::data_dir().join("drivers")
}

/// A cable pack saved by a previous installation, if any (newest first).
pub fn cached_pack() -> Option<PathBuf> {
    let rd = std::fs::read_dir(pack_cache_dir()).ok()?;
    let mut zips: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")))
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p))
        .collect();
    zips.sort_by(|a, b| b.0.cmp(&a.0));
    zips.into_iter().next().map(|(_, p)| p)
}

fn cable_device_count() -> usize {
    let mut keys: Vec<String> = devices::list(Flow::Render, STATE_ACTIVE | STATE_DISABLED).iter().filter(|d| d.is_vb_cable()).map(|d| d.device_key()).collect();
    keys.sort();
    keys.dedup();
    keys.len()
}

fn read_inf_string(dir: &Path, key: &str) -> Option<String> {
    let mut infs = Vec::new();
    find_files(dir, &|n: &str| {
        let l = n.to_lowercase();
        l.ends_with("64_win10.inf") || l.ends_with("64_win7.inf")
    }, &mut infs);
    let inf = infs.into_iter().find(|p| p.parent() == Some(dir))?;
    let text = std::fs::read(&inf).ok()?;
    let text = if text.len() >= 2 && text[0] == 0xFF && text[1] == 0xFE {
        let w: Vec<u16> = text[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&w)
    } else {
        String::from_utf8_lossy(&text).into_owned()
    };
    for line in text.lines() {
        let t = line.trim();
        if !t.to_lowercase().starts_with(&key.to_lowercase()) {
            continue;
        }
        let Some(eq) = t.find('=') else { continue };
        if t[..eq].trim().len() != key.len() {
            continue;
        }
        return Some(t[eq + 1..].trim().trim_matches('"').to_string());
    }
    None
}

/// Device Manager problem code of a device by name (0 = working, 39 = driver not loaded, -1 = not found).
fn device_problem_code(name: &str) -> i32 {
    let script = format!(
        "$d = Get-CimInstance Win32_PnPEntity | Where-Object {{ $_.Name -eq '{}' }} | Select-Object -First 1; if ($d) {{ [int]$d.ConfigManagerErrorCode }} else {{ -1 }}",
        name.replace('\'', "''")
    );
    powershell(&script, Duration::from_secs(30)).and_then(|(_, o)| o.trim().parse().ok()).unwrap_or(-1)
}

/// Installs one more virtual cable from a VB-CABLE A+B pack (zip from VB-Audio after the donation).
fn install_cable_pack(pack: &Path, messages: &mut Vec<String>, result: &mut SetupResult) -> Result<(), String> {
    if !pack.exists() {
        return Err(loc::fmt("Setup.PackNotFound", &[&pack.to_string_lossy()]));
    }
    let work = temp_dir("DualMix-pack");
    let r = (|| -> Result<(), String> {
        unzip(pack, &work)?;
        // The A+B pack contains one zip per cable.
        let mut inner = Vec::new();
        find_files(&work, &|n: &str| n.to_lowercase().ends_with(".zip"), &mut inner);
        for z in inner {
            let dir = z.with_extension("");
            unzip(&z, &dir)?;
        }
        let mut setups = Vec::new();
        find_files(&work, &|n: &str| n.eq_ignore_ascii_case("VBCABLE_Setup_x64.exe"), &mut setups);
        setups.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        if setups.is_empty() {
            return Err(loc::get("Setup.PackEmpty"));
        }
        for setup in setups {
            let dir = setup.parent().unwrap_or(Path::new(".")).to_path_buf();
            let device_name = read_inf_string(&dir, "DeviceName").unwrap_or_else(|| "VB-Audio Cable".into());
            if devices::list(Flow::Render, STATE_ACTIVE | STATE_DISABLED).iter().any(|d| d.interface_name.eq_ignore_ascii_case(&device_name)) {
                continue; // this cable is already there
            }
            let dname = dir.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let signer = verify_signature(&setup).ok_or_else(|| loc::fmt("Setup.PackSigInvalid", &[&dname]))?;
            if !signer.to_lowercase().contains("burel") {
                return Err(loc::fmt("Setup.VbUnexpectedSigner", &[&signer]));
            }
            let before = cable_device_count();
            let defaults = DefaultsSnapshot::take();
            log::info(format!("Installing '{device_name}' from {} (signer: {signer})", pack.display()));
            let exit = run_setup_program(&setup, &["-i", "-h"])?;
            log::info(format!("'{device_name}' setup exit code {exit}"));
            let appeared = wait_for(|| cable_device_count() > before, Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(1500));
            defaults.restore();
            let problem = device_problem_code(&device_name);
            if !appeared || problem > 0 {
                // Windows did not start the driver (typically an old driver blocked by "Memory integrity").
                log::warn(format!("'{device_name}' did not load (endpoint appeared: {appeared}, problem code {problem}): removing it"));
                let _ = run_setup_program(&setup, &["-u", "-h"]);
                result.pack_rejected = true;
                let code = if problem != 0 { format!(" ({problem})") } else { String::new() };
                messages.push(loc::fmt("Setup.PackRejected", &[&device_name, &code]));
                return Ok(());
            }
            messages.push(loc::fmt("Setup.CableInstalled", &[&device_name]));
            result.installed = true;
        }
        // With two bit-transparent cables the free VB-CABLE is not needed any more.
        let transparent = devices::find_buses(&devices::list_render()).iter().filter(|d| !d.is_lossy_cable()).count();
        if transparent >= 2 && remove_cables(false) > 0 {
            messages.push(loc::get("Setup.FreeRemoved"));
        }
        // Keep a copy: DualMix can then reinstall the cable by itself.
        let cache = pack_cache_dir();
        let _ = std::fs::create_dir_all(&cache);
        if let Some(name) = pack.file_name() {
            let cached = cache.join(name);
            if cached != pack {
                if let Err(e) = std::fs::copy(pack, &cached) {
                    log::warn(format!("Cannot cache the cable pack | {e}"));
                }
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    r
}

/// Uninstalls VB-Audio cable devices with their driver packages. `all` false: only the free VB-CABLE.
fn remove_cables(all: bool) -> usize {
    let filter = if all {
        "$_.FriendlyName -like 'VB-Audio*Cable*' -and $_.FriendlyName -notlike '*Voicemeeter*'"
    } else {
        "$_.FriendlyName -eq 'VB-Audio Virtual Cable'"
    };
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue';$devs = @(Get-PnpDevice -Class MEDIA | Where-Object {{ {filter} }});$n = 0;foreach ($d in $devs) {{  $inf = (Get-CimInstance Win32_PnPSignedDriver | Where-Object {{ $_.DeviceID -eq $d.InstanceId }} | Select-Object -First 1).InfName;  pnputil /remove-device $d.InstanceId | Out-Null;  if ($inf -like 'oem*.inf') {{ pnputil /delete-driver $inf /uninstall /force | Out-Null }};  $n++ }};$n"
    );
    let defaults = DefaultsSnapshot::take();
    let out = powershell(&script, Duration::from_secs(90));
    std::thread::sleep(Duration::from_millis(1500));
    defaults.restore();
    let n = out.as_ref().and_then(|(_, o)| o.trim().lines().last().and_then(|l| l.trim().parse().ok())).unwrap_or(0);
    log::info(format!("Cable removal (all={all}): {n} device(s)"));
    n
}

/// Undoes `configure`: factory names back, every endpoint of the cables enabled again.
pub fn unconfigure_endpoints() -> bool {
    let mut changed = false;
    for flow in [Flow::Render, Flow::Capture] {
        for d in devices::list(flow, STATE_ACTIVE | STATE_DISABLED).iter().filter(|d| d.is_vb_cable()) {
            if d.state == STATE_DISABLED && devices::set_visible(&d.id, true) {
                changed = true;
            }
            if flow == Flow::Render && d.description.to_lowercase().starts_with("dualmix") {
                let name = if d.is_main_jack() { d.factory_name() } else { devices::SECONDARY_CABLE_NAME.to_string() };
                if devices::rename(&d.id, &name) {
                    changed = true;
                }
            }
        }
    }
    if changed {
        log::info("Cable endpoints restored to their factory names");
    }
    changed
}

/// Restarts the Windows audio services (and Creative's, which depend on them).
pub fn repair_audio_services() {
    let script = "$ErrorActionPreference='SilentlyContinue';$v = Get-Service CtxSvc32,CTAudSvcService | Where-Object Status -eq 'Running';$v | Stop-Service -Force;Restart-Service AudioEndpointBuilder -Force;Start-Service Audiosrv;Start-Sleep -Seconds 2;$v | Start-Service;Get-Service AudioEndpointBuilder,Audiosrv | ForEach-Object { \"$($_.Name)=$($_.Status)\" }";
    let out = powershell(script, Duration::from_secs(60)).map(|(_, o)| o).unwrap_or_default();
    log::info(format!("Audio services restarted: {}", out.split_whitespace().collect::<Vec<_>>().join(" ")));
}

// ================================================================== endpoint configuration

#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigureResult {
    pub changed: bool,
    pub failed: bool,
}

/// Prepares the cable endpoints: names, secondary inputs disabled, recording side hidden, device format matching
/// the real output (same rate; Game also the same speaker layout). Safe to call repeatedly.
pub fn configure(s: &Settings, elevated: bool, messages: Option<&mut Vec<String>>) -> ConfigureResult {
    let (mut changed, mut failed) = (false, false);

    // 1. One usable jack per cable device.
    let all: Vec<DeviceInfo> = devices::list(Flow::Render, STATE_ACTIVE | STATE_DISABLED).into_iter().filter(|d| d.is_vb_cable()).collect();
    let mut keys: Vec<String> = all.iter().map(|d| d.device_key()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        let mut group: Vec<&DeviceInfo> = all.iter().filter(|d| d.device_key() == k).collect();
        group.sort_by_key(|d| if d.is_main_jack() { 0 } else { 1 });
        let main = group[0];
        if main.state == STATE_DISABLED {
            if devices::set_visible(&main.id, true) {
                changed = true;
                log::info(format!("Enabled '{}'", main.name));
            } else {
                failed = true;
            }
        }
        for secondary in group.iter().skip(1) {
            if secondary.description.to_lowercase().starts_with("dualmix") {
                if devices::rename(&secondary.id, devices::SECONDARY_CABLE_NAME) {
                    changed = true;
                } else {
                    failed = true;
                }
            }
            if secondary.state == STATE_ACTIVE {
                if devices::set_visible(&secondary.id, false) {
                    changed = true;
                    log::info(format!("Disabled secondary cable input '{}'", secondary.name));
                } else {
                    failed = true;
                }
            }
        }
    }
    if changed {
        std::thread::sleep(Duration::from_millis(400)); // let Windows publish the new endpoint states
    }

    // 2. Names and formats.
    let render = devices::list_render();
    let (game, chat) = devices::find_cables(&render);
    let Some(chat) = chat else { return ConfigureResult { changed, failed: true } };
    let output = if s.output_chosen_by_user && render.iter().any(|d| Some(&d.id) == s.output_device_id.as_ref()) {
        render.iter().find(|d| Some(&d.id) == s.output_device_id.as_ref()).cloned()
    } else {
        let pref = s.defaults_backup.as_ref().and_then(|b| b.multimedia.clone()).or_else(|| devices::get_default_id(Role::Multimedia, Flow::Render));
        devices::find_preferred_output(&render, pref.as_deref()).cloned()
    };
    if let Some(g) = &game {
        if g.description != devices::GAME_ENDPOINT_NAME {
            if devices::rename(&g.id, devices::GAME_ENDPOINT_NAME) { changed = true } else { failed = true }
        }
    }
    if chat.description != devices::CHAT_ENDPOINT_NAME {
        if devices::rename(&chat.id, devices::CHAT_ENDPOINT_NAME) { changed = true } else { failed = true }
    }
    for cap in devices::list(Flow::Capture, STATE_ACTIVE).iter().filter(|d| d.is_vb_cable()) {
        if devices::set_visible(&cap.id, false) {
            changed = true;
            log::info(format!("Hidden cable recording endpoint '{}'", cap.name));
        } else {
            failed = true;
        }
    }
    for unused in devices::find_buses(&render).iter().filter(|d| d.id != chat.id && Some(&d.id) != game.as_ref().map(|g| &g.id)) {
        if unused.description.to_lowercase().starts_with("dualmix") && devices::rename(&unused.id, &unused.factory_name()) {
            changed = true;
        }
    }
    if let Some(out_fmt) = output.as_ref().and_then(|o| devices::get_device_format(&o.id)) {
        // Chat is voice: stereo. Game (with its own cable) follows the real output: same rate and speaker layout.
        changed |= align_format(&chat.id, "Chat", out_fmt.sample_rate, &[(2, 0x3)]);
        if let Some(g) = &game {
            changed |= align_format(&g.id, "Game", out_fmt.sample_rate, &layouts(out_fmt.channels, out_fmt.channel_mask));
        }
    }
    if changed {
        log::info(format!("Cable endpoints configured (elevated={elevated}, failed={failed})"));
        if let Some(m) = messages {
            m.push(if game.is_some() { loc::get("Setup.ConfiguredBoth") } else { loc::get("Setup.ConfiguredChat") });
        }
    }
    ConfigureResult { changed, failed }
}

/// Speaker layouts to try for a channel count, the output's own first, stereo last.
fn layouts(channels: usize, mask: u32) -> Vec<(usize, u32)> {
    let mut list = vec![(channels, mask)];
    let mut add = |c: usize, m: u32| {
        if !list.contains(&(c, m)) {
            list.push((c, m));
        }
    };
    if channels == 6 {
        add(6, 0x3F);
        add(6, 0x60F);
    }
    if channels == 8 {
        add(8, 0x63F);
        add(8, 0xFF);
        add(6, 0x3F);
    }
    if channels == 4 {
        add(4, 0x33);
        add(4, 0x603);
    }
    add(2, 0x3);
    list
}

fn align_format(id: &str, label: &str, rate: u32, layouts: &[(usize, u32)]) -> bool {
    let current = devices::get_device_format(id);
    let (want_ch, want_mask) = layouts[0];
    let matches = |c: &AudioFormat, ch: usize, mask: u32| c.sample_rate == rate && c.channels == ch && (c.channel_mask == mask || ch <= 2);
    if current.as_ref().is_some_and(|c| matches(c, want_ch, want_mask)) {
        return false;
    }
    // The engine mixes in float and DualMix captures that float mix (loopback): only rate/layout must match.
    for &(ch, mask) in layouts {
        if let Some(c) = &current {
            if matches(c, ch, mask) {
                log::info(format!("{label} cable keeps {} (preferred layout not accepted by the driver)", format::describe(c)));
                return false;
            }
        }
        let mk = |bits: u16, valid: u16, float: bool| AudioFormat { channels: ch, sample_rate: rate, bits, valid_bits: valid, is_float: float, channel_mask: mask };
        for f in [mk(32, 24, false), mk(24, 24, false), mk(32, 32, false), mk(32, 32, true), mk(16, 16, false)] {
            if !devices::set_device_format(id, &f) {
                continue;
            }
            if let Some(after) = devices::get_device_format(id) {
                if after.channels == ch && after.sample_rate == rate {
                    log::info(format!("{label} cable format set to {}", format::describe(&after)));
                    return true;
                }
            }
        }
    }
    // Layout not changeable: at least match the sample rate with the channels it already has.
    if let Some(c) = &current {
        if c.sample_rate != rate {
            for (bits, valid) in [(32u16, 24u16), (24, 24), (16, 16)] {
                let f = AudioFormat { sample_rate: rate, bits, valid_bits: valid, is_float: false, ..*c };
                if devices::set_device_format(id, &f) && devices::get_device_format(id).is_some_and(|a| a.sample_rate == rate) {
                    log::info(format!("{label} cable sample rate set to {rate} Hz ({} ch kept)", c.channels));
                    return true;
                }
            }
        }
    }
    log::warn(format!(
        "{label} cable format could not be aligned to {rate} Hz (current: {})",
        current.map(|c| format::describe(&c)).unwrap_or_else(|| "?".into())
    ));
    false
}

/// Default playback/recording devices before a driver installation.
struct DefaultsSnapshot(Vec<(Flow, Role, Option<String>)>);

impl DefaultsSnapshot {
    fn take() -> Self {
        let mut v = Vec::new();
        for flow in [Flow::Render, Flow::Capture] {
            for role in [Role::Console, Role::Multimedia, Role::Communications] {
                v.push((flow, role, devices::get_default_id(role, flow)));
            }
        }
        Self(v)
    }

    fn restore(&self) {
        for (flow, role, id) in &self.0 {
            let Some(id) = id else { continue };
            if devices::get_default_id(*role, *flow).as_deref() == Some(id.as_str()) || !devices::is_active(Some(id)) {
                continue;
            }
            if devices::set_default(id, *role) {
                log::info(format!("Default {flow:?}/{role:?} restored after driver installation"));
            }
        }
    }
}

// ================================================================== unprivileged side

/// Runs DualMix elevated (UAC prompt) to install the driver and/or repair the audio service. Blocking: call it
/// from a worker thread.
pub fn run_elevated_blocking(exe: &Path, install: bool, repair: bool, pack: Option<&Path>) -> SetupResult {
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HSTRING, PCWSTR};

    let result_file = std::env::temp_dir().join(format!("dualmix-setup-{:x}.json", crate::router::tick_ms() as u64 ^ (std::process::id() as u64) << 24));
    let mut args = format!("--setup \"{}\"", result_file.display());
    if install {
        args.push_str(" --install");
    }
    if repair {
        args.push_str(" --repair");
    }
    if let Some(p) = pack {
        args.push_str(&format!(" --pack \"{}\"", p.display()));
    }
    log::info(format!("Elevated setup requested (install={install}, repair={repair}, pack={})", pack.map(|p| p.display().to_string()).unwrap_or("-".into())));
    let (verb, file, params) = (HSTRING::from("runas"), HSTRING::from(exe.as_os_str()), HSTRING::from(args));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                log::warn("Elevated setup declined by the user (UAC)");
                return SetupResult { declined: true, message: loc::get("Setup.UacDeclined"), ..Default::default() };
            }
            log::error(format!("Elevated setup failed to start | {e}"));
            return SetupResult { message: e.message().to_string(), ..Default::default() };
        }
        if info.hProcess.is_invalid() {
            return SetupResult { message: loc::get("Setup.CantStart"), ..Default::default() };
        }
        let w = WaitForSingleObject(info.hProcess, 6 * 60 * 1000);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if w == WAIT_TIMEOUT {
            return SetupResult { message: loc::get("Setup.Timeout"), ..Default::default() };
        }
    }
    match std::fs::read_to_string(&result_file) {
        Ok(text) => {
            let _ = std::fs::remove_file(&result_file);
            let r: SetupResult = serde_json::from_str(&text).unwrap_or_default();
            log::info(format!("Elevated setup result: ok={} installed={} repaired={} '{}'", r.ok, r.installed, r.repaired, r.message));
            r
        }
        Err(_) => SetupResult { message: loc::get("Setup.NoResult"), ..Default::default() },
    }
}
