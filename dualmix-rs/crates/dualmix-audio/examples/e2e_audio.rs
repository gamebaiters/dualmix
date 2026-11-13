//! Real end-to-end WASAPI audio & session routing verification on dev-win (or any PC with virtual cables).
//! Renders real WASAPI 440 Hz (Game) + 1000 Hz (Chat) sine waves into virtual buses, captures mixed output via
//! `AudioRouter::tap`, measures Goertzel magnitudes across Mix/Mute transitions, verifies 0 underruns,
//! and exercises `SessionEngine` (`ISimpleAudioVolume` + `AudioPolicyConfig` per-app routing + clean restore).

#[cfg(windows)]
fn main() -> Result<(), String> {
    use std::f64::consts::PI;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use dualmix_audio::com::ComGuard;
    use dualmix_audio::router::{AudioRouter, RouterState};
    use dualmix_audio::sessions::{SessionEngine, SessionParams};
    use dualmix_audio::{LatencyProfile, app_routing, devices};
    use dualmix_core::math::{self, ChannelGains};
    use dualmix_core::rules::RuleSet;
    use dualmix_core::settings::{LatencyMode, Settings};
    use windows::Win32::Media::Audio::{AUDCLNT_SHAREMODE_SHARED, IAudioClient, IAudioRenderClient};
    use windows::Win32::System::Com::{CLSCTX_ALL, CoTaskMemFree};

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--tone") {
        let _g = ComGuard::mta();
        let dev_id = args.get(2).ok_or("missing device id")?.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_tone(dev_id, 660.0, 0.08, Arc::clone(&stop));
        std::thread::sleep(Duration::from_secs(12));
        stop.store(true, Ordering::Release);
        let _ = h.join();
        return Ok(());
    }

    let _com = ComGuard::mta();
    unsafe {
        std::env::set_var("DUALMIX_TEST_REAL_OUTPUT", "Voicemeeter AUX");
    }

    let all = devices::list_render();
    let out = devices::find_preferred_output(&all, None).ok_or("no preferred output found")?;
    let (_game_cable, chat_cable) = devices::find_cables(&all);
    let chat_bus = chat_cable.as_ref().ok_or("no DualMix Chat virtual cable found")?;

    // Optional second virtual endpoint for true dual-bus WASAPI loopback mixing test:
    let game_vm = all
        .iter()
        .find(|d| d.id != out.id && d.id != chat_bus.id && d.name.contains("Voicemeeter In 2"));

    println!(
        "Audio E2E setup:\n  output = {} ({})\n  chat   = {} ({})\n  game   = {}",
        out.name,
        out.id,
        chat_bus.name,
        chat_bus.id,
        game_vm.map(|d| d.name.as_str()).unwrap_or("<direct>")
    );

    fn spawn_tone(device_id: String, freq_hz: f64, amp: f32, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || unsafe {
            let _g = ComGuard::mta();
            let dev = devices::get_device(&device_id).expect("tone GetDevice");
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).expect("tone Activate");
            let mix = client.GetMixFormat().expect("GetMixFormat");
            let ch = (*mix).nChannels as usize;
            let sr = (*mix).nSamplesPerSec as f64;
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix, None)
                .expect("tone Initialize");
            CoTaskMemFree(Some(mix as *const _));
            let buf_frames = client.GetBufferSize().expect("GetBufferSize");
            let render: IAudioRenderClient = client.GetService().expect("IAudioRenderClient");
            let mut phase = 0.0f64;
            let step = 2.0 * PI * freq_hz / sr;

            let avail = buf_frames.min((sr * 0.04) as u32);
            if avail > 0 {
                let ptr = render.GetBuffer(avail).expect("prefill GetBuffer") as *mut f32;
                for i in 0..avail as usize {
                    let s = (phase.sin() as f32) * amp;
                    phase = (phase + step) % (2.0 * PI);
                    for c in 0..ch {
                        *ptr.add(i * ch + c) = s;
                    }
                }
                render.ReleaseBuffer(avail, 0).expect("prefill ReleaseBuffer");
            }
            client.Start().expect("tone Start");

            while !stop.load(Ordering::Relaxed) {
                let pad = client.GetCurrentPadding().unwrap_or(buf_frames);
                let free = buf_frames.saturating_sub(pad);
                if free > 0 {
                    let ptr = render.GetBuffer(free).expect("GetBuffer") as *mut f32;
                    for i in 0..free as usize {
                        let s = (phase.sin() as f32) * amp;
                        phase = (phase + step) % (2.0 * PI);
                        for c in 0..ch {
                            *ptr.add(i * ch + c) = s;
                        }
                    }
                    render.ReleaseBuffer(free, 0).expect("ReleaseBuffer");
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            let _ = client.Stop();
        })
    }

    fn goertzel_mag(samples: &[f32], sr: f64, target_hz: f64) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let n = samples.len() as f64;
        let k = (0.5 + (n * target_hz) / sr).floor();
        let w = (2.0 * PI * k) / n;
        let coeff = 2.0 * w.cos();
        let (mut q1, mut q2) = (0.0f64, 0.0f64);
        for &x in samples {
            let q0 = coeff * q1 - q2 + x as f64;
            q2 = q1;
            q1 = q0;
        }
        let mag2 = (q1 * q1 + q2 * q2 - q1 * q2 * coeff).max(0.0);
        (mag2.sqrt() * 2.0 / n) as f32
    }

    // --- PART 1: Dual-bus WASAPI loopback -> 160-tap SincResampler -> gain/mix -> real output ---
    if let Some(gdev) = game_vm {
        println!("\n=== PART 1: AudioRouter dual-bus WASAPI loopback + mixing ===");
        let cap: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let cap_on = Arc::new(AtomicBool::new(false));
        let cap2 = Arc::clone(&cap);
        let cap_on2 = Arc::clone(&cap_on);

        let mut router = AudioRouter::new();
        router.tap = Some(Arc::new(move |interleaved: &[f32], frames: usize, ch: usize| {
            if !cap_on2.load(Ordering::Relaxed) || ch == 0 {
                return;
            }
            if let Ok(mut g) = cap2.try_lock() {
                for i in 0..frames {
                    g.push(interleaved[i * ch]);
                }
            }
        }));

        let stop_tones = Arc::new(AtomicBool::new(false));
        let tg = spawn_tone(gdev.id.clone(), 440.0, 0.30, Arc::clone(&stop_tones));
        let tc = spawn_tone(chat_bus.id.clone(), 1000.0, 0.30, Arc::clone(&stop_tones));
        std::thread::sleep(Duration::from_millis(250));

        router.start(
            Some(&gdev.id),
            &chat_bus.id,
            &out.id,
            LatencyProfile::for_mode(LatencyMode::Bilanciata),
        )?;
        for _ in 0..40 {
            if router.state() == RouterState::Running {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if router.state() != RouterState::Running {
            return Err(format!("Router failed to reach Running: {:?}", router.last_error()));
        }
        let sr = router.output_format().map(|f| f.sample_rate as f64).unwrap_or(48_000.0);
        println!(
            "Router running: sr={sr}, latency={:.1}ms, rt={}",
            router.latency_ms(),
            router.real_time_scheduling()
        );

        let measure = |label: &str, mix: f64, g_vol: f64, c_vol: f64, g_mute: bool, c_mute: bool| -> (f32, f32) {
            let mut s = Settings::default();
            s.mix = mix;
            s.game_volume = g_vol;
            s.chat_volume = c_vol;
            s.game_muted = g_mute;
            s.chat_muted = c_mute;
            let gains: ChannelGains = math::compute(&s);
            router.set_gains(gains);
            std::thread::sleep(Duration::from_millis(260));
            cap.lock().unwrap().clear();
            cap_on.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(450));
            cap_on.store(false, Ordering::Release);
            let mono = cap.lock().unwrap().clone();
            let m440 = goertzel_mag(&mono, sr, 440.0);
            let m1000 = goertzel_mag(&mono, sr, 1000.0);
            println!(
                "  {label:<24} gains=({:.3},{:.3}) samples={:<5} 440Hz={:.4}  1000Hz={:.4}  underruns={}",
                gains.game,
                gains.chat,
                mono.len(),
                m440,
                m1000,
                router.underruns()
            );
            (m440, m1000)
        };

        let (b440, b1000) = measure("Balanced (mix=0)", 0.0, 1.0, 1.0, false, false);
        assert!(b440 > 0.15 && b1000 > 0.15, "Balanced must pass both tones: {b440}, {b1000}");

        let (g440, g1000) = measure("All Game (mix=-1)", -1.0, 1.0, 1.0, false, false);
        assert!(g440 > 0.15 && g1000 < 0.002, "All Game must mute 1000Hz: {g440}, {g1000}");

        let (c440, c1000) = measure("All Chat (mix=+1)", 1.0, 1.0, 1.0, false, false);
        assert!(c1000 > 0.15 && c440 < 0.002, "All Chat must mute 440Hz: {c440}, {c1000}");

        let (m440, m1000) = measure("ChatMuted + Game 50%", 0.0, 0.5, 1.0, false, true);
        assert!(m440 > 0.01 && m440 < b440 * 0.5 && m1000 < 0.002, "Scaled/muted mismatch: {m440}, {m1000}");

        let (z440, z1000) = measure("Both Muted", 0.0, 1.0, 1.0, true, true);
        assert!(z440 < 0.002 && z1000 < 0.002, "Both muted must be silent: {z440}, {z1000}");

        assert!(router.underruns() <= 10, "Too many VM underruns: {}", router.underruns());
        println!(
            "  Router stats: underruns={}, output_starved={}, game_empty={}, chat_empty={}, chat_drift={:+.1}ppm, learned_q={:.1}ms",
            router.underruns(),
            router.output_starved(),
            router.game_empty(),
            router.chat_empty(),
            router.chat_drift_ppm(),
            router.learned_queue_ms()
        );
        router.stop();
        stop_tones.store(true, Ordering::Release);
        let _ = tg.join();
        let _ = tc.join();
        println!(
            "PART 1 PASS: Goertzel tone isolation + adaptive reserve verified (output_starved={}, learned_q={:.1}ms)",
            router.output_starved(),
            router.learned_queue_ms()
        );
    }

    // --- PART 2: SessionEngine active stream discovery + AudioPolicyConfig per-app routing + restore ---
    println!("\n=== PART 2: SessionEngine active stream discovery + AudioPolicyConfig routing ===");
    let exe_self = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child_raw = std::process::Command::new(exe_self)
        .args(["--tone", &out.id])
        .spawn()
        .map_err(|e| format!("spawn tone child: {e}"))?;
    let child_pid = child_raw.id();
    let _child_guard = ChildGuard(child_raw);
    std::thread::sleep(Duration::from_millis(500));

    let s_cfg = Settings::default();
    let rules = Arc::new(RuleSet::from_settings(&s_cfg));
    let mut se = SessionEngine::new(SessionParams {
        gains: ChannelGains { game: 0.4, chat: 1.0 },
        rules: Arc::clone(&rules),
        rules_gen: 1,
        route_devices: true,
        game_route_id: Some(out.id.clone()),
        chat_route_id: Some(chat_bus.id.clone()),
        direct_route_id: Some(out.id.clone()),
        game_bus_id: None,
        chat_bus_id: Some(chat_bus.id.clone()),
        meters_enabled: true,
    });
    se.start();

    let mut found_self = None;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        let snap = se.snapshot();
        if let Some(app) = snap.iter().find(|a| a.key.contains("e2e_audio") && a.active) {
            found_self = Some(app.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let app = found_self.ok_or("SessionEngine did not discover active e2e_audio child session")?;
    println!(
        "  Discovered child session (pid={child_pid}): key='{}', ch={:?}, peak={:.4}, dev='{}'",
        app.key, app.channel, app.peak, app.devices
    );
    assert!(app.peak > 0.005, "Expected live IAudioMeterInformation peak > 0.005, got {}", app.peak);

    // Move child app explicitly to Chat channel via updated RuleSet and verify AudioPolicyConfig!
    let mut s_chat = s_cfg.clone();
    s_chat.extra_chat_apps.push(app.key.clone());
    let chat_rules = Arc::new(RuleSet::from_settings(&s_chat));
    se.update(SessionParams {
        gains: ChannelGains { game: 0.4, chat: 1.0 },
        rules: chat_rules,
        rules_gen: 2,
        route_devices: true,
        game_route_id: Some(out.id.clone()),
        chat_route_id: Some(chat_bus.id.clone()),
        direct_route_id: Some(out.id.clone()),
        game_bus_id: None,
        chat_bus_id: Some(chat_bus.id.clone()),
        meters_enabled: true,
    });
    std::thread::sleep(Duration::from_millis(700));
    let routed_dev = app_routing::get_app_device(child_pid);
    println!("  AudioPolicyConfig route for child PID {child_pid}: {routed_dev:?}");
    assert_eq!(routed_dev.as_deref(), Some(chat_bus.id.as_str()), "Expected child app routed to Chat bus");

    let _ = se.unroute_all(vec![]).recv_timeout(Duration::from_secs(2));
    se.stop(true);
    let routed_after = app_routing::get_app_device(child_pid);
    println!("  AudioPolicyConfig route after stop(true): {routed_after:?}");
    assert!(
        routed_after.as_deref().unwrap_or("").is_empty(),
        "Expected AudioPolicyConfig route cleared/default on clean stop, got {routed_after:?}"
    );

    drop(_child_guard);
    println!("PART 2 PASS: SessionEngine live meter + AudioPolicyConfig route/restore verified!");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
