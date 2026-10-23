//! Runtime encoder / codec preference. host_settings.json beats the environment,
//! and "auto" or a missing field falls through. Illegal values are ignored.
//! Auto follows `auto_order` (NVENC then CPU when NVIDIA is present, else CPU).
//! A finished background probe can replace that choice; an unfinished probe keeps
//! the legacy NVIDIA/CPU result so the first connection is not delayed.

use crate::video::Encoder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Setting,
    Env,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecPref {
    Auto,
    H264,
    H265,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub encoder: Encoder,
    pub encoder_source: Source,
    pub codec_pref: CodecPref,
    pub codec_source: Source,
}

pub fn parse_encoder(value: Option<&str>) -> Option<Option<Encoder>> {
    match value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
    {
        None => Some(None),
        Some(v) if v == "auto" => Some(None),
        Some(v) if v == "cpu" => Some(Some(Encoder::Cpu)),
        Some(v) if v == "vaapi" => Some(Some(Encoder::Vaapi)),
        Some(v) if v == "nvenc" => Some(Some(Encoder::Nvenc)),
        Some(_) => None,
    }
}

pub fn parse_codec(value: Option<&str>) -> Option<Option<CodecPref>> {
    match value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
    {
        None => Some(None),
        Some(v) if v == "auto" => Some(None),
        Some(v) if v == "h264" => Some(Some(CodecPref::H264)),
        Some(v) if v == "h265" || v == "hevc" => Some(Some(CodecPref::H265)),
        Some(_) => None,
    }
}

fn file_fields() -> (Option<String>, Option<String>) {
    let path = crate::system_actions::settings_path();
    let Ok(bytes) = std::fs::read(path) else { return (None, None) };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return (None, None) };
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_owned);
    (field("video_encoder"), field("video_codec"))
}

/// Unset/auto tries NVENC only with an NVIDIA driver loaded: `ffmpeg -encoders` lists h264_nvenc
/// on any machine, so without this every stream start first ran a doomed NVENC process (#10).
pub fn legacy_auto(nvidia: bool) -> Encoder {
    if nvidia { Encoder::Nvenc } else { Encoder::Cpu }
}

/// Auto candidate order. NVIDIA present: NVENC then CPU. No NVIDIA: CPU only.
/// VAAPI is not an auto candidate until #33 (FFmpeg-process VAAPI ignores maxrate).
/// Explicit vaapi (setting or `UU_VIDEO_ENCODER`) is unchanged.
pub fn auto_order(nvidia: bool) -> Vec<Encoder> {
    if nvidia { vec![Encoder::Nvenc, Encoder::Cpu] } else { vec![Encoder::Cpu] }
}

/// Why one candidate's synthetic IDR did not count. Logged as the variant name only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeFail { Timeout, NoIdr, Spawn, Exit }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Ok,
    Fail(ProbeFail),
    /// inproc-av only: the FFmpeg process probe failed, but this session would try the native
    /// engine first (`read_encoder_with`) on an X11 display (not Wayland, portal, or KMS).
    /// Opening that engine needs a display, so the candidate is not failed here; auto falls
    /// back to the pre-probe NVIDIA/CPU choice for it.
    NativeUnprobed,
    /// Single auto candidate (CPU, no NVIDIA): the synthetic IDR is skipped.
    NotProbed,
}

impl ProbeOutcome {
    /// Counts as a successful probe for `choose_from_probe`.
    /// `NativeUnprobed` does not: that candidate was not shown to encode.
    pub fn ok(self) -> bool { matches!(self, Self::Ok) }
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fail(ProbeFail::Timeout) => "timeout",
            Self::Fail(ProbeFail::NoIdr) => "no_idr",
            Self::Fail(ProbeFail::Spawn) => "spawn",
            Self::Fail(ProbeFail::Exit) => "exit",
            Self::NativeUnprobed => "native_unprobed",
            Self::NotProbed => "not probed (only candidate)",
        }
    }
}

/// How a process-probe result becomes the stored outcome.
/// `native_would_run` is true only on inproc-av when `inproc_encoder::selected` is true
/// (hardware, not `UU_VIDEO_PIPELINE=process` / `UU_CAPTURE=grab` / a bundled FFmpeg).
/// A process failure then becomes `NativeUnprobed` instead of disqualifying that candidate.
/// Default builds pass `native_would_run = false`, so the process result is unchanged.
pub fn outcome_for_candidate(process: ProbeOutcome, native_would_run: bool) -> ProbeOutcome {
    if process.ok() || !native_would_run { process } else { ProbeOutcome::NativeUnprobed }
}

/// Encoder auto should use when a hardware candidate was left `NativeUnprobed`.
/// That candidate is skipped by `choose_from_probe`; if it was the legacy pick (NVENC when
/// NVIDIA is present), keep the legacy encoder. Otherwise use the probe choice.
pub fn chosen_after_native(nvidia: bool, order: &[Encoder], outcomes: &[ProbeOutcome], probed: Encoder) -> Encoder {
    let legacy = legacy_auto(nvidia);
    let native_legacy = order.iter().zip(outcomes.iter()).any(|(e, o)| {
        *e == legacy && matches!(o, ProbeOutcome::NativeUnprobed)
    });
    if native_legacy { legacy } else { probed }
}

/// How long a hardware-candidate failure stays cached. A boot-time NVENC miss
/// (module not ready, GPU busy, consumer session limit) must not stick until exit.
pub const PROBE_FAILURE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub order: Vec<Encoder>,
    pub outcomes: Vec<ProbeOutcome>,
    pub chosen: Encoder,
    /// `None` means the result does not expire (success, or CPU-only `NotProbed`).
    pub expires_at: Option<std::time::Instant>,
    /// `nvidia_present()` at the time the report was stored. A later desktop
    /// connection whose presence differs treats the report as stale.
    pub nvidia: bool,
}

impl ProbeReport {
    pub fn fresh(order: Vec<Encoder>, outcomes: Vec<ProbeOutcome>, chosen: Encoder, now: std::time::Instant, nvidia: bool) -> Self {
        let expires_at = probe_failure_deadline(&order, &outcomes, chosen, now);
        Self { order, outcomes, chosen, expires_at, nvidia }
    }
}

/// Expiry is only for the hardware candidate that caused the CPU fallback
/// (NVENC `Fail` and `chosen == Cpu`). A `Fail` on CPU while NVENC is `Ok`
/// does not expire, and doctor does not label that report a failure.
/// `NotProbed` and `Ok` do not. `NativeUnprobed` keeps the legacy encoder
/// and is not a failure cache.
pub fn probe_failure_deadline(
    order: &[Encoder],
    outcomes: &[ProbeOutcome],
    chosen: Encoder,
    now: std::time::Instant,
) -> Option<std::time::Instant> {
    let hardware_failed = order.iter().zip(outcomes).any(|(encoder, outcome)| {
        *encoder != Encoder::Cpu && matches!(outcome, ProbeOutcome::Fail(_))
    });
    (hardware_failed && chosen == Encoder::Cpu).then_some(now + PROBE_FAILURE_TTL)
}

pub fn probe_expired(report: &ProbeReport, now: std::time::Instant) -> bool {
    report.expires_at.is_some_and(|at| now >= at)
}

/// Time-to-live elapsed, or NVIDIA presence no longer matches the report.
/// Presence is `nvidia_present()` (proc version or `/dev/nvidia0`).
pub fn probe_stale(report: &ProbeReport, now: std::time::Instant, nvidia: bool) -> bool {
    probe_expired(report, now) || report.nvidia != nvidia
}

/// Log suffix for a probe that replaces `previous`. `None` is a first probe.
/// A failure deadline (`expires_at`) is "retry after failure". A presence
/// mismatch without that deadline is "NVIDIA availability changed".
pub fn reprobe_reason(previous: Option<&ProbeReport>, nvidia_now: bool) -> Option<&'static str> {
    let previous = previous?;
    if previous.expires_at.is_some() {
        Some("retry after failure")
    } else if previous.nvidia != nvidia_now {
        Some("NVIDIA availability changed")
    } else {
        None
    }
}

/// Empty cache, or the cached report is stale for `nvidia` at `now`.
/// Callers still use the cached encoder until the replacement probe is stored.
pub fn probe_needs_run(now: std::time::Instant, nvidia: bool) -> bool {
    match cached_probe() {
        None => true,
        Some(report) => probe_stale(&report, now, nvidia),
    }
}

/// Background warm-up (startup and each new desktop connection) runs only for auto.
/// Setting or `UU_VIDEO_ENCODER` (non-auto) matches v0.2.23: no GPU probe.
pub fn should_warm_auto_probe(source: Source) -> bool {
    source == Source::Auto
}

/// First candidate that produced an IDR, else CPU.
pub fn choose_from_probe(order: &[Encoder], outcomes: &[ProbeOutcome]) -> Encoder {
    order.iter().zip(outcomes.iter()).find_map(|(e, o)| o.ok().then_some(*e)).unwrap_or(Encoder::Cpu)
}

fn cache() -> &'static std::sync::Mutex<Option<ProbeReport>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<ProbeReport>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

pub fn store_probe(report: ProbeReport) {
    if let Ok(mut slot) = cache().lock() { *slot = Some(report); }
}

pub fn cached_probe() -> Option<ProbeReport> {
    cache().lock().ok().and_then(|g| g.clone())
}

/// Tests that call `clear_probe` / `store_probe` share this lock. The probe
/// cache is process-global and `cargo test` runs tests in parallel.
#[cfg(test)]
pub(crate) static PROBE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Tests that read or bump the process-global reload generation share this lock.
#[cfg(test)]
pub(crate) static RELOAD_TEST_LOCK:std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub fn clear_probe() {
    if let Ok(mut slot) = cache().lock() { *slot = None; }
}

/// Why a live desktop encoder was asked to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchReason {
    /// Effective encoder differs. The log line uses `Selection::encoder_source`, not this variant.
    Encoder,
    /// Codec preference changed. The negotiated codec stays until the next stream.
    CodecDeferred,
}

/// What SIGUSR2 should do to one live desktop stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchPlan {
    /// Restart the encoder. None keeps the running encoder (codec-only or no change).
    pub encoder: Option<Encoder>,
    pub reason: SwitchReason,
    /// Chroma the restarted encoder can actually produce. VAAPI has no 4:4:4.
    pub chroma: u64,
    /// True when the request was 4:4:4 and the new encoder must report 4:2:0.
    pub chroma_downgraded: bool,
}

/// Compare the selection this stream is using with a freshly read one.
/// Codec preference is the stored preference, not the negotiated m-line codec.
pub fn plan_switch(active_encoder: Encoder, previous_codec: CodecPref, next: &Selection, active_chroma: u64) -> Option<SwitchPlan> {
    if next.encoder != active_encoder {
        let chroma = if active_chroma == crate::capture_setting::CHROMA_444 && !crate::video::full_chroma_for(next.encoder) {
            crate::capture_setting::CHROMA_420
        } else {
            active_chroma
        };
        return Some(SwitchPlan {
            encoder: Some(next.encoder),
            reason: SwitchReason::Encoder,
            chroma,
            chroma_downgraded: chroma != active_chroma,
        });
    }
    if next.codec_pref != previous_codec {
        return Some(SwitchPlan {
            encoder: None,
            reason: SwitchReason::CodecDeferred,
            chroma: active_chroma,
            chroma_downgraded: false,
        });
    }
    None
}

/// Monotonic generation bumped by SIGUSR2. Streams watch it and re-read `selection()`.
fn reload_generation() -> &'static std::sync::atomic::AtomicU64 {
    static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    &GEN
}

pub fn reload_generation_value() -> u64 {
    reload_generation().load(std::sync::atomic::Ordering::Relaxed)
}

/// SIGUSR2: ask live desktop streams to re-read host_settings. Does not clear the probe cache.
pub fn notify_reload() {
    reload_generation().fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn reload_watch() -> &'static tokio::sync::watch::Sender<u64> {
    static WATCH: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> = std::sync::OnceLock::new();
    WATCH.get_or_init(|| {
        let (tx, _) = tokio::sync::watch::channel(reload_generation_value());
        tx
    })
}

pub fn subscribe_reload() -> tokio::sync::watch::Receiver<u64> {
    reload_watch().subscribe()
}

/// True when SIGUSR2 has bumped the generation since the session snapshot.
/// A background auto probe that finishes during setup does not bump it.
pub fn reload_since_session(at_session: u64, current: u64) -> bool {
    current != at_session
}

/// Publish the generation so every subscribed desktop stream wakes.
pub fn publish_reload() {
    notify_reload();
    // send() drops the value when nobody is subscribed (the gap between desktop connections).
    reload_watch().send_replace(reload_generation_value());
}

/// Setting (non-auto) beats the environment, which beats the NVIDIA/CPU default.
pub fn selection() -> Selection {
    selection_with(cached_probe().as_ref())
}

/// Same as `selection`, but auto uses `report` when it is already cached.
/// `None` keeps the legacy NVIDIA/CPU result (the probe is still running or has not started).
pub fn selection_with(report: Option<&ProbeReport>) -> Selection {
    let (file_enc, file_codec) = file_fields();
    let env_enc = std::env::var("UU_VIDEO_ENCODER").ok();
    let env_codec = std::env::var("UU_VIDEO_CODEC").ok();
    resolve_pair(file_enc.as_deref(), env_enc.as_deref(), file_codec.as_deref(), env_codec.as_deref(), crate::video::nvidia_present(), report)
}

pub fn resolve_pair(
    enc_setting: Option<&str>,
    enc_env: Option<&str>,
    codec_setting: Option<&str>,
    codec_env: Option<&str>,
    nvidia: bool,
    probed: Option<&ProbeReport>,
) -> Selection {
    let (encoder, encoder_source) = match parse_encoder(enc_setting) {
        Some(Some(e)) => (e, Source::Setting),
        None => {
            log::warn!("Ignoring video_encoder value (not auto/cpu/vaapi/nvenc)");
            pick_encoder(enc_env, nvidia, probed)
        }
        Some(None) => pick_encoder(enc_env, nvidia, probed),
    };
    let (codec_pref, codec_source) = match parse_codec(codec_setting) {
        Some(Some(p)) => (p, Source::Setting),
        None => {
            log::warn!("Ignoring video_codec value (not auto/h264/h265)");
            pick_codec(codec_env)
        }
        Some(None) => pick_codec(codec_env),
    };
    Selection {
        encoder,
        encoder_source,
        codec_pref,
        codec_source,
    }
}

fn pick_encoder(env: Option<&str>, nvidia: bool, probed: Option<&ProbeReport>) -> (Encoder, Source) {
    match parse_encoder(env) {
        Some(Some(e)) => (e, Source::Env),
        None => {
            log::warn!("Ignoring UU_VIDEO_ENCODER value (not auto/cpu/vaapi/nvenc)");
            (auto_encoder(nvidia, probed), Source::Auto)
        }
        Some(None) => (auto_encoder(nvidia, probed), Source::Auto),
    }
}

fn auto_encoder(nvidia: bool, probed: Option<&ProbeReport>) -> Encoder {
    probed.map(|p| p.chosen).unwrap_or_else(|| legacy_auto(nvidia))
}

fn pick_codec(env: Option<&str>) -> (CodecPref, Source) {
    match parse_codec(env) {
        Some(Some(p)) => (p, Source::Env),
        None => {
            log::warn!("Ignoring UU_VIDEO_CODEC value (not auto/h264/h265)");
            (CodecPref::Auto, Source::Auto)
        }
        Some(None) => (CodecPref::Auto, Source::Auto),
    }
}

pub fn codec_force(pref: CodecPref) -> Option<&'static str> {
    match pref {
        CodecPref::H264 => Some("h264"),
        CodecPref::H265 => Some("h265"),
        CodecPref::Auto => None,
    }
}

/// Hardware (VAAPI / NVENC) prefers H.265 unless the preference forces a codec.
pub fn hardware(encoder: Encoder) -> bool {
    encoder != Encoder::Cpu
}

pub fn source_label(source: Source) -> &'static str {
    match source {
        Source::Setting => "setting",
        Source::Env => "env",
        Source::Auto => "default",
    }
}

pub fn encoder_source_label(source: Source) -> &'static str {
    match source {
        Source::Setting => "setting",
        Source::Env => "env",
        Source::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(
        es: Option<&str>,
        ee: Option<&str>,
        cs: Option<&str>,
        ce: Option<&str>,
        nvidia: bool,
    ) -> Selection {
        resolve_pair(es, ee, cs, ce, nvidia, None)
    }

    #[test]
    fn priority_matrix_and_illegal_values() {
        let encoders = [
            None,
            Some("auto"),
            Some("cpu"),
            Some("vaapi"),
            Some("nvenc"),
            Some("bogus"),
        ];
        let codecs = [None, Some("auto"), Some("h264"), Some("h265"), Some("nope")];
        for setting in encoders {
            for env in encoders {
                let got = sel(setting, env, None, None, true);
                let expect_enc = match (parse_encoder(setting), parse_encoder(env)) {
                    (Some(Some(e)), _) => (e, Source::Setting),
                    (None, Some(Some(e))) | (Some(None), Some(Some(e))) => (e, Source::Env),
                    _ => (Encoder::Nvenc, Source::Auto),
                };
                assert_eq!(
                    (got.encoder, got.encoder_source),
                    expect_enc,
                    "{setting:?} {env:?}"
                );
            }
        }
        for setting in codecs {
            for env in codecs {
                let got = sel(None, None, setting, env, false);
                let expect = match (parse_codec(setting), parse_codec(env)) {
                    (Some(Some(p)), _) => (p, Source::Setting),
                    (None, Some(Some(p))) | (Some(None), Some(Some(p))) => (p, Source::Env),
                    _ => (CodecPref::Auto, Source::Auto),
                };
                assert_eq!(
                    (got.codec_pref, got.codec_source),
                    expect,
                    "{setting:?} {env:?}"
                );
            }
        }
        assert_eq!(
            sel(Some("auto"), Some("cpu"), Some("auto"), Some("h264"), true).encoder,
            Encoder::Cpu
        );
        assert_eq!(
            sel(Some("auto"), None, None, None, false).encoder,
            Encoder::Cpu
        );
        assert_eq!(
            sel(None, Some("auto"), None, None, true).encoder,
            Encoder::Nvenc
        );
        assert_eq!(
            sel(Some("  NVENC "), None, Some(" HEVC "), None, false).encoder,
            Encoder::Nvenc
        );
        assert_eq!(
            sel(None, None, Some(" HEVC "), None, false).codec_pref,
            CodecPref::H265
        );
    }

    #[test]
    fn process_failure_keeps_native_candidate_unprobed() {
        let exit = ProbeOutcome::Fail(ProbeFail::Exit);
        assert_eq!(outcome_for_candidate(ProbeOutcome::Ok, true), ProbeOutcome::Ok);
        assert_eq!(outcome_for_candidate(exit, false), exit, "default build keeps the process result");
        assert_eq!(outcome_for_candidate(exit, true), ProbeOutcome::NativeUnprobed);
        assert!(!ProbeOutcome::NativeUnprobed.ok());
        assert_eq!(ProbeOutcome::NativeUnprobed.label(), "native_unprobed");
    }

    #[test]
    fn native_unprobed_hardware_uses_legacy_choice() {
        let order = auto_order(true);
        let outcomes = [ProbeOutcome::NativeUnprobed, ProbeOutcome::Ok];
        let probed = choose_from_probe(&order, &outcomes);
        assert_eq!(probed, Encoder::Cpu);
        assert_eq!(chosen_after_native(true, &order, &outcomes, probed), Encoder::Nvenc);
        let cpu_only = auto_order(false);
        assert_eq!(chosen_after_native(false, &cpu_only, &[ProbeOutcome::Ok], Encoder::Cpu), Encoder::Cpu);
        let both_ok = [ProbeOutcome::Ok, ProbeOutcome::Ok];
        assert_eq!(chosen_after_native(true, &order, &both_ok, Encoder::Nvenc), Encoder::Nvenc);
    }

    fn probe_report(nvidia: bool, outcomes: &[ProbeOutcome]) -> ProbeReport {
        let order = auto_order(nvidia);
        let chosen = choose_from_probe(&order, outcomes);
        ProbeReport::fresh(order, outcomes.to_vec(), chosen, std::time::Instant::now(), nvidia)
    }

    #[test]
    fn auto_order_keeps_nvenc_when_nvidia_is_present() {
        assert_eq!(auto_order(true), vec![Encoder::Nvenc, Encoder::Cpu]);
        assert_eq!(auto_order(false), vec![Encoder::Cpu]);
        assert_eq!(legacy_auto(true), Encoder::Nvenc);
        assert_eq!(legacy_auto(false), Encoder::Cpu);
    }

    #[test]
    fn auto_falls_back_when_the_first_candidate_fails() {
        let both = probe_report(true, &[ProbeOutcome::Fail(ProbeFail::NoIdr), ProbeOutcome::Fail(ProbeFail::Exit)]);
        assert_eq!(both.chosen, Encoder::Cpu);
        let nv = probe_report(true, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        assert_eq!(nv.chosen, Encoder::Nvenc);
        let failed = probe_report(true, &[ProbeOutcome::Fail(ProbeFail::Timeout), ProbeOutcome::Ok]);
        assert_eq!(failed.chosen, Encoder::Cpu);
        assert_eq!(resolve_pair(None, None, None, None, true, Some(&nv)).encoder, Encoder::Nvenc);
        assert_eq!(resolve_pair(None, None, None, None, true, Some(&failed)).encoder, Encoder::Cpu);
        let cpu = probe_report(false, &[ProbeOutcome::Ok]);
        assert_eq!(cpu.chosen, Encoder::Cpu);
        assert_eq!(resolve_pair(None, None, None, None, false, Some(&cpu)).encoder, Encoder::Cpu);
        let skipped = probe_report(false, &[ProbeOutcome::NotProbed]);
        assert_eq!(skipped.chosen, Encoder::Cpu);
        assert_eq!(ProbeOutcome::NotProbed.label(), "not probed (only candidate)");
        assert!(!ProbeOutcome::NotProbed.ok());
        assert_eq!(resolve_pair(None, None, None, None, false, Some(&skipped)).encoder, Encoder::Cpu);
    }

    #[test]
    fn explicit_env_and_setting_ignore_the_probe() {
        let va = probe_report(true, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        assert_eq!(resolve_pair(None, Some("nvenc"), None, None, false, Some(&va)).encoder, Encoder::Nvenc);
        assert_eq!(resolve_pair(None, Some("cpu"), None, None, true, Some(&va)).encoder, Encoder::Cpu);
        assert_eq!(resolve_pair(Some("vaapi"), Some("nvenc"), None, None, true, Some(&va)).encoder, Encoder::Vaapi);
        assert_eq!(resolve_pair(Some("nvenc"), None, None, None, false, Some(&va)).encoder_source, Source::Setting);
        assert_eq!(resolve_pair(None, Some("cpu"), None, None, false, Some(&va)).encoder_source, Source::Env);
        assert_eq!(resolve_pair(Some("auto"), Some("vaapi"), None, None, false, None).encoder, Encoder::Vaapi);
        assert_eq!(resolve_pair(None, None, None, Some("h264"), false, Some(&va)).codec_pref, CodecPref::H264);
    }

    fn selection_of(encoder: Encoder, codec: CodecPref) -> Selection {
        Selection { encoder, encoder_source: Source::Setting, codec_pref: codec, codec_source: Source::Setting }
    }

    #[test]
    fn live_switch_restarts_encoder_and_defers_codec() {
        let nv = selection_of(Encoder::Nvenc, CodecPref::Auto);
        assert!(plan_switch(Encoder::Nvenc, CodecPref::Auto, &nv, crate::capture_setting::CHROMA_420).is_none());
        let to_cpu = plan_switch(Encoder::Nvenc, CodecPref::Auto, &selection_of(Encoder::Cpu, CodecPref::Auto), crate::capture_setting::CHROMA_444).unwrap();
        assert_eq!(to_cpu.encoder, Some(Encoder::Cpu));
        assert_eq!(to_cpu.reason, SwitchReason::Encoder);
        assert!(!to_cpu.chroma_downgraded);
        assert_eq!(to_cpu.chroma, crate::capture_setting::CHROMA_444);
        let to_vaapi = plan_switch(Encoder::Nvenc, CodecPref::H265, &selection_of(Encoder::Vaapi, CodecPref::H265), crate::capture_setting::CHROMA_444).unwrap();
        assert_eq!(to_vaapi.encoder, Some(Encoder::Vaapi));
        assert!(to_vaapi.chroma_downgraded);
        assert_eq!(to_vaapi.chroma, crate::capture_setting::CHROMA_420);
        let codec = plan_switch(Encoder::Nvenc, CodecPref::Auto, &selection_of(Encoder::Nvenc, CodecPref::H264), crate::capture_setting::CHROMA_420).unwrap();
        assert_eq!(codec.encoder, None);
        assert_eq!(codec.reason, SwitchReason::CodecDeferred);
        let both = plan_switch(Encoder::Cpu, CodecPref::Auto, &selection_of(Encoder::Nvenc, CodecPref::H264), crate::capture_setting::CHROMA_420).unwrap();
        assert_eq!(both.encoder, Some(Encoder::Nvenc));
        assert_eq!(both.reason, SwitchReason::Encoder);
        // The log reason is the source of the new selection (setting / env / auto), not a fixed word.
        for (source, label) in [(Source::Setting, "setting"), (Source::Env, "env"), (Source::Auto, "auto")] {
            let mut next = selection_of(Encoder::Vaapi, CodecPref::Auto);
            next.encoder_source = source;
            let plan = plan_switch(Encoder::Nvenc, CodecPref::Auto, &next, crate::capture_setting::CHROMA_420).unwrap();
            assert!(plan.encoder.is_some());
            assert_eq!(encoder_source_label(next.encoder_source), label);
        }
    }

    #[test]
    fn reload_does_not_clear_the_probe_cache() {
        let _reload = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = PROBE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let report = probe_report(true, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        store_probe(report.clone());
        let before = reload_generation_value();
        notify_reload();
        assert_eq!(reload_generation_value(), before + 1);
        assert_eq!(cached_probe(), Some(report));
        assert!(reload_since_session(before, reload_generation_value()));
        assert!(!reload_since_session(reload_generation_value(), reload_generation_value()));
        clear_probe();
    }

    #[test]
    fn probe_finish_is_not_a_reload_since_the_session_snapshot() {
        let _reload = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let at_session = reload_generation_value();
        // A finished background probe updates the cache only. It does not bump the generation.
        assert!(!reload_since_session(at_session, reload_generation_value()));
        notify_reload();
        assert!(reload_since_session(at_session, reload_generation_value()));
    }

    #[test]
    fn mark_reload_seen_clears_has_changed() {
        let _reload = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut watch = subscribe_reload();
            let _ = watch.borrow_and_update();
            publish_reload();
            assert!(watch.has_changed().unwrap());
            let _ = watch.borrow_and_update();
            assert!(!watch.has_changed().unwrap());
        });
    }

    /// Codec at CONTROL open is `preferred_codec_for` on the session snapshot.
    /// Auto + CPU uses H.264 when both codecs are negotiated; auto + NVENC keeps H.265.
    /// Forced codec ignores the encoder. 真彩 does not switch codec (true_color_keeps_codec_and_vaapi_reports_420).
    #[test]
    fn auto_codec_follows_the_encoder_chosen_at_session_setup() {
        use crate::capture_setting::{preferred_codec_for, VideoCodec};

        let cpu_fallback = probe_report(true, &[ProbeOutcome::Fail(ProbeFail::NoIdr), ProbeOutcome::Ok]);
        let session = resolve_pair(None, None, None, None, true, Some(&cpu_fallback));
        assert_eq!((session.encoder, session.encoder_source, session.codec_pref), (Encoder::Cpu, Source::Auto, CodecPref::Auto));
        assert_eq!(preferred_codec_for(true, true, session), VideoCodec::H264);
        // Not the H.265 an unfinished probe would pick for legacy NVENC.
        let early_nvenc = resolve_pair(None, None, None, None, true, None);
        assert_eq!(early_nvenc.encoder, Encoder::Nvenc);
        assert_eq!(preferred_codec_for(true, true, early_nvenc), VideoCodec::H265);

        let nvenc_ok = probe_report(true, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        let nvenc = resolve_pair(None, None, None, None, true, Some(&nvenc_ok));
        assert_eq!(nvenc.encoder, Encoder::Nvenc);
        assert_eq!(preferred_codec_for(true, true, nvenc), VideoCodec::H265);

        let order = auto_order(true);
        let unprobed = [ProbeOutcome::NativeUnprobed, ProbeOutcome::Ok];
        let chosen = chosen_after_native(true, &order, &unprobed, choose_from_probe(&order, &unprobed));
        let report = ProbeReport::fresh(order.clone(), unprobed.to_vec(), chosen, std::time::Instant::now(), true);
        let native = resolve_pair(None, None, None, None, true, Some(&report));
        assert_eq!(native.encoder, Encoder::Nvenc);
        assert_eq!(preferred_codec_for(true, true, native), VideoCodec::H265);

        let forced_h264 = resolve_pair(None, None, Some("h264"), None, true, Some(&nvenc_ok));
        assert_eq!(forced_h264.encoder, Encoder::Nvenc);
        assert_eq!(preferred_codec_for(true, true, forced_h264), VideoCodec::H264);
        let forced_h265 = resolve_pair(None, None, None, Some("h265"), true, Some(&cpu_fallback));
        assert_eq!(forced_h265.encoder, Encoder::Cpu);
        assert_eq!(preferred_codec_for(true, true, forced_h265), VideoCodec::H265);
    }

    #[test]
    fn session_before_probe_uses_legacy_auto() {
        let _guard = PROBE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_probe();
        assert!(cached_probe().is_none());
        let early = resolve_pair(None, None, None, None, false, None);
        assert_eq!((early.encoder, early.encoder_source), (Encoder::Cpu, Source::Auto));
        assert_eq!(resolve_pair(None, None, None, None, true, None).encoder, Encoder::Nvenc);
        store_probe(probe_report(true, &[ProbeOutcome::Fail(ProbeFail::Exit), ProbeOutcome::Ok]));
        assert_eq!(cached_probe().unwrap().chosen, Encoder::Cpu);
        assert_eq!(resolve_pair(None, None, None, None, true, cached_probe().as_ref()).encoder, Encoder::Cpu);
        // A snapshot taken before the cache filled stays on the legacy encoder.
        assert_eq!(early.encoder, Encoder::Cpu);
        assert_eq!(resolve_pair(None, None, None, None, true, None).encoder, Encoder::Nvenc);
        clear_probe();
    }

    #[test]
    fn success_and_not_probed_do_not_expire() {
        let now = std::time::Instant::now();
        let later = now + PROBE_FAILURE_TTL + std::time::Duration::from_secs(1);
        let ok = ProbeReport::fresh(auto_order(true), vec![ProbeOutcome::Ok, ProbeOutcome::Ok], Encoder::Nvenc, now, true);
        assert!(ok.expires_at.is_none());
        assert!(!probe_expired(&ok, later));
        let skipped = ProbeReport::fresh(auto_order(false), vec![ProbeOutcome::NotProbed], Encoder::Cpu, now, false);
        assert!(skipped.expires_at.is_none());
        assert!(!probe_expired(&skipped, later));
        let native = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::NativeUnprobed, ProbeOutcome::Ok],
            Encoder::Nvenc,
            now,
            true,
        );
        assert!(native.expires_at.is_none(), "native_unprobed is not a recorded process failure");
        assert!(!probe_expired(&native, later));
    }

    #[test]
    fn hardware_failure_expires_after_ttl() {
        let now = std::time::Instant::now();
        let failed = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::Fail(ProbeFail::Timeout), ProbeOutcome::Ok],
            Encoder::Cpu,
            now,
            true,
        );
        assert!(!probe_expired(&failed, now + PROBE_FAILURE_TTL - std::time::Duration::from_secs(1)));
        assert!(probe_expired(&failed, now + PROBE_FAILURE_TTL));
        assert_eq!(failed.chosen, Encoder::Cpu);
    }

    /// NVENC produced an IDR and CPU failed: the session uses NVENC, so this is
    /// not the hardware-fallback cache. No TTL, and doctor (which keys off
    /// `expires_at`) does not append the failure label.
    #[test]
    fn nvenc_ok_cpu_fail_does_not_expire() {
        let now = std::time::Instant::now();
        let report = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::Ok, ProbeOutcome::Fail(ProbeFail::Exit)],
            Encoder::Nvenc,
            now,
            true,
        );
        assert_eq!(report.chosen, Encoder::Nvenc);
        assert!(report.expires_at.is_none());
        assert!(!probe_expired(&report, now + PROBE_FAILURE_TTL + std::time::Duration::from_secs(1)));
        let cpu_only_fail = ProbeReport::fresh(
            auto_order(false),
            vec![ProbeOutcome::Fail(ProbeFail::Exit)],
            Encoder::Cpu,
            now,
            false,
        );
        assert!(cpu_only_fail.expires_at.is_none(), "a CPU-only Fail is not a hardware fallback");
    }

    /// Failed NVENC stays CPU until `PROBE_FAILURE_TTL`. After that a desktop connection
    /// asks for one background re-probe (`probe_needs_run`, what `warm_auto_probe` checks)
    /// and still selects the cached CPU encoder. A successful re-probe selects NVENC and
    /// does not expire. A second failure restarts the TTL.
    #[test]
    fn failure_then_reprobe_success_selects_nvenc() {
        let _guard = PROBE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_probe();
        let t0 = std::time::Instant::now();
        let failed = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::Fail(ProbeFail::Exit), ProbeOutcome::Ok],
            Encoder::Cpu,
            t0,
            true,
        );
        store_probe(failed);
        let before = t0 + PROBE_FAILURE_TTL - std::time::Duration::from_secs(1);
        assert!(!probe_needs_run(before, true), "no re-probe before the failure expires");
        assert_eq!(
            resolve_pair(None, None, None, None, true, cached_probe().as_ref()).encoder,
            Encoder::Cpu
        );
        let after = t0 + PROBE_FAILURE_TTL;
        assert!(probe_expired(cached_probe().as_ref().unwrap(), after));
        assert!(probe_needs_run(after, true), "desktop connection starts one background re-probe");
        assert_eq!(
            resolve_pair(None, None, None, None, true, cached_probe().as_ref()).encoder,
            Encoder::Cpu,
            "the connection that notices expiry still uses the cached encoder"
        );
        let retried = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::Ok, ProbeOutcome::Ok],
            Encoder::Nvenc,
            after,
            true,
        );
        store_probe(retried);
        assert_eq!(
            resolve_pair(None, None, None, None, true, cached_probe().as_ref()).encoder,
            Encoder::Nvenc
        );
        assert!(!probe_needs_run(after + PROBE_FAILURE_TTL + std::time::Duration::from_secs(1), true));
        let failed_again = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::Fail(ProbeFail::Timeout), ProbeOutcome::Ok],
            Encoder::Cpu,
            after,
            true,
        );
        store_probe(failed_again);
        assert!(!probe_needs_run(after + PROBE_FAILURE_TTL - std::time::Duration::from_secs(1), true));
        assert!(probe_needs_run(after + PROBE_FAILURE_TTL, true));
        clear_probe();
    }

    /// A CPU-only `NotProbed` report does not expire by time. When NVIDIA appears
    /// (module or device after the user service started), the next desktop
    /// connection treats it as stale and `probe_needs_run` asks for one background
    /// re-probe. The inverse (NVENC success, device gone) is stale the same way.
    /// The connection that notices still selects the cached encoder.
    #[test]
    fn nvidia_presence_change_makes_probe_stale() {
        let now = std::time::Instant::now();
        let cpu_only = ProbeReport::fresh(auto_order(false), vec![ProbeOutcome::NotProbed], Encoder::Cpu, now, false);
        assert!(!probe_stale(&cpu_only, now + PROBE_FAILURE_TTL + std::time::Duration::from_secs(1), false));
        assert!(probe_stale(&cpu_only, now, true), "NVIDIA appeared after the CPU-only skip");
        let nvenc_ok = ProbeReport::fresh(auto_order(true), vec![ProbeOutcome::Ok, ProbeOutcome::Ok], Encoder::Nvenc, now, true);
        assert!(!probe_stale(&nvenc_ok, now, true));
        assert!(probe_stale(&nvenc_ok, now, false), "NVIDIA gone after a successful NVENC probe");

        let _guard = PROBE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_probe();
        store_probe(cpu_only);
        assert!(!probe_needs_run(now, false));
        assert!(probe_needs_run(now, true));
        assert_eq!(
            resolve_pair(None, None, None, None, true, cached_probe().as_ref()).encoder,
            Encoder::Cpu,
            "the connection that notices the presence change still uses the cached encoder"
        );
        clear_probe();
    }

    #[test]
    fn reprobe_reason_follows_previous_state() {
        let now = std::time::Instant::now();
        assert_eq!(reprobe_reason(None, false), None);
        assert_eq!(reprobe_reason(None, true), None);

        let failed = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::Fail(ProbeFail::Exit), ProbeOutcome::Ok],
            Encoder::Cpu,
            now,
            true,
        );
        assert!(failed.expires_at.is_some());
        assert_eq!(reprobe_reason(Some(&failed), true), Some("retry after failure"));
        assert_eq!(reprobe_reason(Some(&failed), false), Some("retry after failure"));

        let nvenc_ok = ProbeReport::fresh(
            auto_order(true),
            vec![ProbeOutcome::Ok, ProbeOutcome::Ok],
            Encoder::Nvenc,
            now,
            true,
        );
        assert_eq!(reprobe_reason(Some(&nvenc_ok), true), None);
        assert_eq!(reprobe_reason(Some(&nvenc_ok), false), Some("NVIDIA availability changed"));

        let skipped = ProbeReport::fresh(auto_order(false), vec![ProbeOutcome::NotProbed], Encoder::Cpu, now, false);
        assert_eq!(reprobe_reason(Some(&skipped), false), None);
        assert_eq!(reprobe_reason(Some(&skipped), true), Some("NVIDIA availability changed"));
        assert_ne!(reprobe_reason(Some(&nvenc_ok), false), Some("retry after failure"));
        assert_ne!(reprobe_reason(Some(&skipped), true), Some("retry after failure"));
    }

    #[test]
    fn warm_probe_only_when_encoder_source_is_auto() {
        assert!(should_warm_auto_probe(Source::Auto));
        assert!(!should_warm_auto_probe(Source::Setting));
        assert!(!should_warm_auto_probe(Source::Env));
        assert_eq!(resolve_pair(Some("cpu"), None, None, None, true, None).encoder_source, Source::Setting);
        assert_eq!(resolve_pair(None, Some("nvenc"), None, None, false, None).encoder_source, Source::Env);
        assert_eq!(resolve_pair(None, Some("auto"), None, None, false, None).encoder_source, Source::Auto);
    }
}
