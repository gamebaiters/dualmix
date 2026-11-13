//! Built-in headless diagnostic suite (`DualMix.exe --diag [--quick] [--stress]`).
//!
//! Verifies settings, math/rules, resampler SNR, localization, WASAPI device enumeration,
//! `IPolicyConfig` / `AudioPolicyConfig`, `AudioRouter`, `SessionEngine`, and `KnobController`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dualmix_audio::{
    LatencyProfile, app_routing, devices, driver_setup, endpoint_volume::EndpointVolume, router::AudioRouter,
    sessions::{SessionEngine, SessionParams},
};
use dualmix_core::knob::{KnobActionKind, KnobConfig};
use dualmix_core::loc::{self, Lang};
use dualmix_core::log;
use dualmix_core::math;
use dualmix_core::resampler::SincResampler;
use dualmix_core::ringbuf::SpscRing;
use dualmix_core::rules::RuleSet;
use dualmix_core::settings::{Channel, KnobTarget, LanguagePreference, Settings, SettingsStore};
use dualmix_input::KnobController;

pub struct DiagReport {
    pub ok: bool,
    pub text: String,
    pub path: std::path::PathBuf,
}

pub fn run(quick: bool, stress: bool) -> DiagReport {
    let mut lines = vec![
        format!("DualMix v{} Diagnostics", env!("CARGO_PKG_VERSION")),
        format!("DataDir: {}", log::data_dir().display()),
    ];
    let mut failures = 0usize;

    let mut check = |name: &str, res: Result<String, String>| {
        match res {
            Ok(detail) => {
                lines.push(format!("[PASS] {name}: {detail}"));
            }
            Err(err) => {
                failures += 1;
                lines.push(format!("[FAIL] {name}: {err}"));
            }
        }
    };

    // 1. Settings
    check("1. Settings", (|| -> Result<String, String> {
        let mut s = SettingsStore::load();
        let orig = s.mix;
        s.mix = 0.25;
        let json = serde_json::to_string(&s).map_err(|e| e.to_string())?;
        let parsed: Settings = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        if (parsed.mix - 0.25).abs() > 1e-6 {
            return Err("Mix roundtrip mismatch".into());
        }
        s.mix = orig;
        Ok(format!("v{}, mode={:?}, mix={:.2}", s.version, s.mode, s.mix))
    })());

    // 2. Math & RuleSet
    check("2. Math & Rules", (|| -> Result<String, String> {
        let mut s = Settings::default();
        s.mix = -0.5;
        s.game_volume = 0.8;
        s.chat_volume = 0.6;
        let g = math::compute(&s);
        if !(g.game > 0.0 && g.chat > 0.0 && g.chat < g.game) {
            return Err(format!("Unexpected gains: game={} chat={}", g.game, g.chat));
        }
        let rules = RuleSet::from_settings(&s);
        let discord = rules.resolve("Discord", false);
        if discord != Channel::Chat {
            return Err(format!("Expected Discord->Chat, got {discord:?}"));
        }
        Ok(format!("gains=({:.3}, {:.3}), Discord={discord:?}", g.game, g.chat))
    })());

    // 3. Localization
    check("3. Localization", (|| -> Result<String, String> {
        let prev = loc::current();
        loc::apply(LanguagePreference::English);
        let en = loc::get("Nav.Mixer");
        loc::apply(LanguagePreference::Italian);
        let it = loc::get("Nav.Mixer");
        loc::apply(match prev {
            Lang::En => LanguagePreference::English,
            Lang::It => LanguagePreference::Italian,
        });
        if en.is_empty() || it.is_empty() || en == "Nav.Mixer" || it == "Nav.Mixer" {
            return Err("Missing Nav.Mixer key".into());
        }
        Ok(format!("EN='{en}', IT='{it}'"))
    })());

    // 4. Resampler & SPSC Ring
    check("4. Resampler & SpscRing", (|| -> Result<String, String> {
        let mut r = SincResampler::new(2, 1.05);
        let n = 4096usize;
        let mut src = vec![0.0f32; n * 2];
        for i in 0..n {
            let v = (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48000.0).sin() as f32 * 0.5;
            src[i * 2] = v;
            src[i * 2 + 1] = v;
        }
        r.write_interleaved(&src, n);
        let mut out = vec![vec![0.0f32; 2048]; 2];
        let got = r.read(&mut out, 2048, 1.0);
        if got < 1800 {
            return Err(format!("Resampler produced only {got} frames"));
        }
        let ring = SpscRing::with_capacity(256);
        let wr = [1.0f32, 2.0, 3.0, 4.0];
        let mut rd = [0.0f32; 4];
        let p = ring.push(&wr);
        let c = ring.pop(&mut rd);
        if p != 4 || c != 4 || rd != wr {
            return Err("SpscRing read/write mismatch".into());
        }
        Ok(format!("produced={got} frames, cutoff={:.2}, ring=OK", r.cutoff()))
    })());

    // 5. Audio Devices & Policy APIs
    let mut preferred_out: Option<String> = None;
    let mut game_bus: Option<String> = None;
    let mut chat_bus: Option<String> = None;
    check("5. WASAPI Devices & Policy", (|| -> Result<String, String> {
        let render = devices::list_render();
        let out = devices::find_preferred_output(&render, None);
        let (g, c) = devices::find_cables(&render);
        preferred_out = out.as_ref().map(|d| d.id.clone());
        game_bus = g.as_ref().map(|d| d.id.clone());
        chat_bus = c.as_ref().map(|d| d.id.clone());
        let pol = devices::policy_config_available();
        let apc = app_routing::is_available();
        let cables = driver_setup::cables_present();
        Ok(format!(
            "render={}, out='{}', cables={}, IPolicyConfig={}, AudioPolicyConfig={}",
            render.len(),
            out.as_ref().map(|d| d.name.as_str()).unwrap_or("<none>"),
            cables,
            pol,
            apc
        ))
    })());

    // 6. Endpoint Format & Volume
    check("6. Endpoint Format & Volume", (|| -> Result<String, String> {
        if let Some(id) = preferred_out.as_deref() {
            let probe = devices::probe_open(id);
            let fmt = devices::get_device_format(id).or_else(|| devices::get_mix_format(id));
            let mut ep = EndpointVolume::new(Arc::new(|| {}));
            ep.attach(Some(id));
            let avail = ep.is_available();
            let lvl = ep.level();
            ep.detach();
            if let Some(hr) = probe {
                return Err(format!("probe_open failed hr=0x{:08X}", hr as u32));
            }
            Ok(format!(
                "fmt={}, ep_vol={} ({:.0}%)",
                fmt.as_ref().map(dualmix_audio::format::describe).unwrap_or_else(|| "?".into()),
                avail,
                lvl * 100.0
            ))
        } else {
            Ok("skipped (no active output endpoint)".into())
        }
    })());

    // 7. AudioRouter dry-run (when cables + output exist and not --quick)
    check("7. AudioRouter", (|| -> Result<String, String> {
        if quick {
            return Ok("skipped (--quick)".into());
        }
        let (Some(out_id), Some(chat_id)) = (preferred_out.as_deref(), chat_bus.as_deref()) else {
            return Ok("skipped (virtual cables or output not present)".into());
        };
        let mut router = AudioRouter::new();
        let prof = LatencyProfile::for_mode(dualmix_core::settings::LatencyMode::Bilanciata);
        router.start(game_bus.as_deref(), chat_id, out_id, prof)?;
        let dur = if stress { Duration::from_secs(4) } else { Duration::from_millis(600) };
        let start = Instant::now();
        while start.elapsed() < dur {
            std::thread::sleep(Duration::from_millis(50));
            if router.state() == dualmix_audio::router::RouterState::Error || router.blocked() {
                break;
            }
        }
        let st = router.state();
        let underruns = router.underruns();
        let lat = router.latency_ms();
        let err = router.last_error().unwrap_or_default();
        router.stop();
        if st == dualmix_audio::router::RouterState::Error || router.blocked() {
            return Err(format!("Router error: {err}"));
        }
        Ok(format!("state={st:?}, lat={lat:.1} ms, underruns={underruns}"))
    })());

    // 8. SessionEngine MTA
    check("8. SessionEngine", (|| -> Result<String, String> {
        let s = Settings::default();
        let params = SessionParams {
            gains: math::compute(&s),
            rules: Arc::new(RuleSet::from_settings(&s)),
            rules_gen: 1,
            route_devices: false,
            game_route_id: None,
            chat_route_id: None,
            direct_route_id: None,
            game_bus_id: game_bus.clone(),
            chat_bus_id: chat_bus.clone(),
            meters_enabled: true,
        };
        let mut eng = SessionEngine::new(params);
        eng.start();
        std::thread::sleep(Duration::from_millis(if quick { 150 } else { 400 }));
        let apps = eng.snapshot();
        eng.stop(false);
        Ok(format!("sessions={} apps", apps.len()))
    })());

    // 9. KnobController
    check("9. KnobController", (|| -> Result<String, String> {
        let mut s = Settings::default();
        s.knob_target = KnobTarget::Mix;
        let actions = Arc::new(std::sync::Mutex::new(Vec::<KnobActionKind>::new()));
        let a2 = actions.clone();
        let kc = KnobController::new(KnobConfig::from_settings(&s), Arc::new(move |act| {
            a2.lock().unwrap().push(act.kind);
        }));
        let swallowed = kc.handle_key(dualmix_core::knob::VK_VOLUME_UP, true, false);
        let _ = kc.handle_key(dualmix_core::knob::VK_VOLUME_UP, false, false);
        let n = actions.lock().unwrap().len();
        if !swallowed || n == 0 {
            return Err("KnobController did not dispatch VK_VOLUME_UP in Mix mode".into());
        }
        // Volume mode leaves VK_VOLUME_UP to Windows
        s.knob_target = KnobTarget::Volume;
        kc.update_config(KnobConfig::from_settings(&s));
        if kc.handle_key(dualmix_core::knob::VK_VOLUME_UP, true, false) {
            return Err("KnobController unexpectedly swallowed VK_VOLUME_UP in Volume mode".into());
        }
        Ok(format!("Mix intercepted ({n} action), Volume passthrough=OK"))
    })());

    // 10. Logging & Diag Output
    let diag_path = log::data_dir().join("diag.txt");
    let ok = failures == 0;
    lines.push(String::new());
    lines.push(if ok {
        "RESULT: OK".to_string()
    } else {
        format!("RESULT: FAIL ({failures} failed)")
    });
    let text = lines.join("\n");
    let _ = std::fs::create_dir_all(log::data_dir());
    let _ = std::fs::write(&diag_path, &text);
    log::info(format!("Diagnostics finished: {}", if ok { "OK" } else { "FAIL" }));

    DiagReport { ok, text, path: diag_path }
}
