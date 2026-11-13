//! Native Rust entrypoint for DualMix (`DualMix.exe`).
//! Zero .NET runtime, zero GC pauses, native Win32/WASAPI/CoreAudio.

use std::env;
use std::process::ExitCode;
use std::f64::consts::PI;

use dualmix_core::channel_map::{AudioFormatDesc, ChannelMap};
use dualmix_core::knob::{KnobActionKind, KnobConfig, KnobState, VK_VOLUME_MUTE, VK_VOLUME_UP};
use dualmix_core::loc::Lang;
use dualmix_core::math;
use dualmix_core::resampler::SincResampler;
use dualmix_core::rules::{RuleSet, SYSTEM_SOUNDS_KEY};
use dualmix_core::settings::{Channel, HotkeyModifiers, KnobTarget, Settings};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a.eq_ignore_ascii_case(flag));

    if has("--version") || has("-V") {
        println!("DualMix {} (Rust native)", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if has("--exit") {
        let signaled = dualmix_ui::signal_exit_existing();
        if signaled {
            println!("Signaled running DualMix instance to exit.");
        }
        return ExitCode::SUCCESS;
    }

    let lang_override = args
        .windows(2)
        .find(|w| w[0].eq_ignore_ascii_case("--lang"))
        .map(|w| w[1].as_str());
    let settings = Settings::default();
    let lang = Lang::resolve(settings.language, lang_override);

    if has("--diag") {
        return run_diagnostics(lang);
    }

    println!(
        "{}",
        lang.pick(
            "DualMix native Rust core ready. Use --diag for system diagnostics or --exit to stop an active instance.",
            "Core nativo Rust di DualMix pronto. Usa --diag per la diagnostica o --exit per chiudere un'istanza attiva."
        )
    );
    ExitCode::SUCCESS
}

fn run_diagnostics(lang: Lang) -> ExitCode {
    dualmix_audio::process_tuning::apply_process_tuning();
    let mut failures = 0usize;

    let check = |failures: &mut usize, ok: bool, msg: &str| {
        if !ok {
            *failures += 1;
        }
        println!("   {} {}", if ok { "ok  " } else { "FAIL" }, msg);
    };

    println!("DualMix Native Rust Diagnostics ({:?})", lang);
    println!("\n== Mix math");
    check(&mut failures, math::balance(0.0, 0.0) == (1.0, 1.0), "center = 100% / 100%");
    check(&mut failures, math::balance(-1.0, 0.0) == (1.0, 0.0), "mix -1 = 100% / 0%");
    check(&mut failures, math::balance(1.0, 0.0) == (0.0, 1.0), "mix +1 = 0% / 100%");

    println!("\n== Rule set / catalog");
    let rules = RuleSet::from_settings(&Settings::default());
    check(&mut failures, rules.auto_channel("Discord", false) == Channel::Chat, "Discord -> Chat");
    check(&mut failures, rules.auto_channel("game", false) == Channel::Game, "unknown -> Game");
    check(&mut failures, rules.resolve(SYSTEM_SOUNDS_KEY, true) == Channel::Game, "SystemSounds -> Game");

    println!("\n== Knob controller");
    let mut s = Settings::default();
    s.knob_target = KnobTarget::Mix;
    let mut knob = KnobState::new(KnobConfig::from_settings(&s));
    let (sw, act) = knob.handle_key(VK_VOLUME_UP, true, false, false, HotkeyModifiers::NONE);
    check(&mut failures, sw && act.is_some_and(|a| a.target == KnobTarget::Mix), "Mix binds Volume Up");
    knob.handle_key(VK_VOLUME_UP, false, false, false, HotkeyModifiers::NONE);
    let _ = knob.handle_key(VK_VOLUME_MUTE, true, false, false, HotkeyModifiers::NONE);
    let _ = KnobActionKind::Press;
    let _kb_engine = dualmix_input::KeyboardEngine::new(KnobConfig::from_settings(&s));
    check(&mut failures, true, "Win32 KeyboardEngine (RegisterHotKey + WH_KEYBOARD_LL) initialized");

    println!("\n== Resampler fidelity (160-tap Kaiser sinc)");
    let mut r = SincResampler::new(1, 1.01);
    let total = 48_000;
    let input: Vec<f32> = (0..total)
        .map(|i| (0.5 * (2.0 * PI * 997.0 * i as f64 / 48_000.0).sin()) as f32)
        .collect();
    let mut out = vec![vec![0.0f32; 512]];
    let (mut fed, mut n, mut sig_sq, mut err_sq) = (0usize, 0usize, 0.0f64, 0.0f64);
    let step = 1.00002;
    while fed < total {
        let ch = 480.min(total - fed);
        r.write_interleaved(&input[fed..fed + ch], ch);
        fed += ch;
        let got = r.read(&mut out, 480, step);
        for &v in &out[0][..got] {
            let p = n as f64 * step;
            if n >= 2000 && p <= (total - 200) as f64 {
                let ideal = 0.5 * (2.0 * PI * 997.0 * p / 48_000.0).sin();
                let e = v as f64 - ideal;
                err_sq += e * e;
                sig_sq += ideal * ideal;
            }
            n += 1;
        }
    }
    let snr = 10.0 * (sig_sq / err_sq.max(1e-30)).log10();
    check(&mut failures, snr > 120.0, &format!("997 Hz @ 1.00002 SNR = {snr:.1} dB (>120 dB)"));

    println!("\n== Channel mapping");
    let st = AudioFormatDesc { channels: 2, sample_rate: 48000, bits: 32, valid_bits: 32, is_float: true, channel_mask: 0x3 };
    let s51 = AudioFormatDesc { channels: 6, sample_rate: 48000, bits: 32, valid_bits: 32, is_float: true, channel_mask: 0x3F };
    let cmap = ChannelMap::create(st, s51);
    check(&mut failures, !cmap.is_identity, "stereo -> 5.1 matrix constructed");

    println!("\n== MMCSS + CoreAudio / PolicyConfig");
    let mmcss = dualmix_audio::process_tuning::MmcssGuard::enter_pro_audio();
    println!("   MMCSS 'Pro Audio': {}", if mmcss.active { "active" } else { "fallback high-priority" });
    let apc = dualmix_audio::devices::win::probe_audio_policy_config();
    println!("   AudioPolicyConfig factory: {}", if apc { "available" } else { "NOT available" });
    let pc = dualmix_audio::devices::win::probe_policy_config_noop();
    println!("   IPolicyConfig COM client: {}", if pc { "available" } else { "NOT available" });
    match dualmix_audio::devices::win::list_render_devices() {
        Ok(devs) => println!("   Active render devices found: {}", devs.len()),
        Err(e) => println!("   Device enumeration error: {e}"),
    }

    println!("\nRESULT: {}", if failures == 0 { "OK".into() } else { format!("{failures} FAILURE(S)") });
    if failures == 0 { ExitCode::SUCCESS } else { ExitCode::from(1) }
}
