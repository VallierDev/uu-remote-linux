//! Runtime encoder / codec preference. host_settings.json beats the environment,
//! and "auto" or a missing field falls through. Illegal values are ignored.
//! Auto follows `auto_order` (NVENC when NVIDIA is present, else VAAPI, else CPU).
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

/// Auto candidate order. NVIDIA present keeps NVENC first and leaves VAAPI out, because the
/// FFmpeg-process VAAPI path still ignores maxrate (#33). With no NVIDIA, VAAPI is tried
/// before CPU. One function so the order can be relaxed after #33.
pub fn auto_order(nvidia: bool) -> Vec<Encoder> {
    if nvidia { vec![Encoder::Nvenc, Encoder::Cpu] } else { vec![Encoder::Vaapi, Encoder::Cpu] }
}

/// Why one candidate's synthetic IDR did not count. Logged as the variant name only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeFail { Timeout, NoIdr, Spawn, Exit }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Ok,
    Fail(ProbeFail),
    /// inproc-av only: the FFmpeg process probe failed, but this session would try the native
    /// engine first (`read_encoder_with`). Opening that engine needs a display, so the candidate
    /// is not failed here; auto falls back to the pre-probe NVIDIA/CPU choice for it.
    NativeUnprobed,
}

impl ProbeOutcome {
    /// Counts as a successful probe for `choose_from_probe` and 真彩 fallback.
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub order: Vec<Encoder>,
    pub outcomes: Vec<ProbeOutcome>,
    pub chosen: Encoder,
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

#[cfg(test)]
pub fn clear_probe() {
    if let Ok(mut slot) = cache().lock() { *slot = None; }
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

/// Auto + 真彩 moves off VAAPI onto the next probed candidate that can do 4:4:4.
/// If none of those produced an IDR, stay on VAAPI and drop to 4:2:0 (`true`).
/// An explicit vaapi setting or `UU_VIDEO_ENCODER=vaapi` stays on VAAPI.
pub fn encoder_for_chroma(selection: Selection, chroma: u64, probed: Option<&ProbeReport>) -> (Encoder, bool) {
    if chroma != crate::capture_setting::CHROMA_444 || selection.encoder != Encoder::Vaapi {
        return (selection.encoder, false);
    }
    if selection.encoder_source != Source::Auto { return (selection.encoder, true); }
    let Some(report) = probed else { return (selection.encoder, true); };
    match report.order.iter().zip(report.outcomes.iter()).find_map(|(e, o)| {
        (o.ok() && *e != Encoder::Vaapi && crate::video::full_chroma_for(*e)).then_some(*e)
    }) {
        Some(encoder) => (encoder, false),
        None => (selection.encoder, true),
    }
}

/// Session snapshot after a 真彩 request. Uses the probe captured with the session, not a later one.
pub fn selection_for_chroma(mut selection: Selection, chroma: u64, probed: Option<&ProbeReport>) -> Selection {
    selection.encoder = encoder_for_chroma(selection, chroma, probed).0;
    selection
}

/// 4:4:4 to list in device_capability. The selected encoder, or (source auto only) a probed
/// fallback that produced an IDR and can do 4:4:4. Explicit vaapi stays 4:2:0.
pub fn advertise_full_chroma(selection: Selection, probed: Option<&ProbeReport>) -> bool {
    let (encoder, drop_420) = encoder_for_chroma(selection, crate::capture_setting::CHROMA_444, probed);
    !drop_420 && crate::video::full_chroma_for(encoder)
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
        let vaapi = auto_order(false);
        let va_out = [ProbeOutcome::NativeUnprobed, ProbeOutcome::Ok];
        let va_probed = choose_from_probe(&vaapi, &va_out);
        assert_eq!(chosen_after_native(false, &vaapi, &va_out, va_probed), Encoder::Cpu);
        let both_ok = [ProbeOutcome::Ok, ProbeOutcome::Ok];
        assert_eq!(chosen_after_native(true, &order, &both_ok, Encoder::Nvenc), Encoder::Nvenc);
    }

    #[test]
    fn probe_chroma_request_moves_auto_vaapi_to_cpu() {
        let report = probe_report(false, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        let base = resolve_pair(None, None, None, None, false, Some(&report));
        assert_eq!(base.encoder, Encoder::Vaapi);
        let moved = selection_for_chroma(base, crate::capture_setting::CHROMA_444, Some(&report));
        assert_eq!(moved.encoder, Encoder::Cpu);
        let kept = selection_for_chroma(base, crate::capture_setting::CHROMA_420, Some(&report));
        assert_eq!(kept.encoder, Encoder::Vaapi);
        let only_vaapi = probe_report(false, &[ProbeOutcome::Ok, ProbeOutcome::Fail(ProbeFail::Exit)]);
        let stuck = resolve_pair(None, None, None, None, false, Some(&only_vaapi));
        assert_eq!(selection_for_chroma(stuck, crate::capture_setting::CHROMA_444, Some(&only_vaapi)).encoder, Encoder::Vaapi);
        // CPU 4:4:4 fallback with codec auto uses the CPU rule (H.264 when negotiated).
        assert_eq!(crate::capture_setting::preferred_codec_for(true, true, moved), crate::capture_setting::VideoCodec::H264);
        assert_eq!(crate::capture_setting::preferred_codec_for(true, true, base), crate::capture_setting::VideoCodec::H265);
        let forced = resolve_pair(None, None, Some("h265"), None, false, Some(&report));
        let forced = selection_for_chroma(forced, crate::capture_setting::CHROMA_444, Some(&report));
        assert_eq!(forced.encoder, Encoder::Cpu);
        assert_eq!(crate::capture_setting::preferred_codec_for(true, true, forced), crate::capture_setting::VideoCodec::H265);
        assert_eq!(crate::capture_setting::preferred_codec_for(false, true, moved), crate::capture_setting::VideoCodec::H265);
    }

    fn probe_report(nvidia: bool, outcomes: &[ProbeOutcome]) -> ProbeReport {
        let order = auto_order(nvidia);
        ProbeReport { chosen: choose_from_probe(&order, outcomes), outcomes: outcomes.to_vec(), order }
    }

    #[test]
    fn auto_order_keeps_nvenc_when_nvidia_is_present() {
        assert_eq!(auto_order(true), vec![Encoder::Nvenc, Encoder::Cpu]);
        assert_eq!(auto_order(false), vec![Encoder::Vaapi, Encoder::Cpu]);
        assert_eq!(legacy_auto(true), Encoder::Nvenc);
        assert_eq!(legacy_auto(false), Encoder::Cpu);
    }

    #[test]
    fn auto_falls_back_when_the_first_candidate_fails() {
        let ok = probe_report(false, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        assert_eq!(ok.chosen, Encoder::Vaapi);
        let failed = probe_report(false, &[ProbeOutcome::Fail(ProbeFail::Timeout), ProbeOutcome::Ok]);
        assert_eq!(failed.chosen, Encoder::Cpu);
        let both = probe_report(true, &[ProbeOutcome::Fail(ProbeFail::NoIdr), ProbeOutcome::Fail(ProbeFail::Exit)]);
        assert_eq!(both.chosen, Encoder::Cpu);
        let nv = probe_report(true, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        assert_eq!(resolve_pair(None, None, None, None, true, Some(&nv)).encoder, Encoder::Nvenc);
        assert_eq!(resolve_pair(None, None, None, None, false, Some(&ok)).encoder, Encoder::Vaapi);
        assert_eq!(resolve_pair(None, None, None, None, false, Some(&failed)).encoder, Encoder::Cpu);
    }

    #[test]
    fn chroma_444_auto_vaapi_moves_only_to_a_probed_candidate() {
        let both = probe_report(false, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        let auto = resolve_pair(None, None, None, None, false, Some(&both));
        assert_eq!(encoder_for_chroma(auto, crate::capture_setting::CHROMA_444, Some(&both)), (Encoder::Cpu, false));
        let cpu_down = probe_report(false, &[ProbeOutcome::Ok, ProbeOutcome::Fail(ProbeFail::Spawn)]);
        assert_eq!(encoder_for_chroma(auto, crate::capture_setting::CHROMA_444, Some(&cpu_down)), (Encoder::Vaapi, true));
        assert_eq!(encoder_for_chroma(auto, crate::capture_setting::CHROMA_420, Some(&both)).0, Encoder::Vaapi);
        let pinned = resolve_pair(Some("vaapi"), None, None, None, false, Some(&both));
        assert_eq!(encoder_for_chroma(pinned, crate::capture_setting::CHROMA_444, Some(&both)), (Encoder::Vaapi, true));
        let from_env = resolve_pair(None, Some("vaapi"), None, None, false, Some(&both));
        assert_eq!(encoder_for_chroma(from_env, crate::capture_setting::CHROMA_444, Some(&both)).0, Encoder::Vaapi);
        let nvidia = probe_report(true, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        let nv = resolve_pair(None, None, None, None, true, Some(&nvidia));
        assert_eq!(encoder_for_chroma(nv, crate::capture_setting::CHROMA_444, Some(&nvidia)).0, Encoder::Nvenc);
        assert!(advertise_full_chroma(auto, Some(&both)));
        assert!(!advertise_full_chroma(auto, Some(&cpu_down)));
        assert!(!advertise_full_chroma(pinned, Some(&both)));
        assert!(!advertise_full_chroma(from_env, Some(&both)));
        assert!(advertise_full_chroma(nv, Some(&nvidia)));
        let early = resolve_pair(None, None, None, None, false, None);
        assert!(advertise_full_chroma(early, None));
    }

    #[test]
    fn explicit_env_and_setting_ignore_the_probe() {
        let va = probe_report(false, &[ProbeOutcome::Ok, ProbeOutcome::Ok]);
        assert_eq!(resolve_pair(None, Some("nvenc"), None, None, false, Some(&va)).encoder, Encoder::Nvenc);
        assert_eq!(resolve_pair(None, Some("cpu"), None, None, true, Some(&va)).encoder, Encoder::Cpu);
        assert_eq!(resolve_pair(Some("vaapi"), Some("nvenc"), None, None, true, Some(&va)).encoder, Encoder::Vaapi);
        assert_eq!(resolve_pair(Some("nvenc"), None, None, None, false, Some(&va)).encoder_source, Source::Setting);
        assert_eq!(resolve_pair(None, Some("cpu"), None, None, false, Some(&va)).encoder_source, Source::Env);
        assert_eq!(resolve_pair(Some("auto"), Some("vaapi"), None, None, false, None).encoder, Encoder::Vaapi);
        assert_eq!(resolve_pair(None, None, None, Some("h264"), false, Some(&va)).codec_pref, CodecPref::H264);
    }

    #[test]
    fn session_before_probe_uses_legacy_auto() {
        clear_probe();
        assert!(cached_probe().is_none());
        let early = resolve_pair(None, None, None, None, false, None);
        assert_eq!((early.encoder, early.encoder_source), (Encoder::Cpu, Source::Auto));
        assert_eq!(resolve_pair(None, None, None, None, true, None).encoder, Encoder::Nvenc);
        store_probe(probe_report(false, &[ProbeOutcome::Ok, ProbeOutcome::Ok]));
        assert_eq!(cached_probe().unwrap().chosen, Encoder::Vaapi);
        assert_eq!(resolve_pair(None, None, None, None, false, cached_probe().as_ref()).encoder, Encoder::Vaapi);
        // A snapshot taken before the cache filled stays on the legacy encoder.
        assert_eq!(selection_for_chroma(early, crate::capture_setting::CHROMA_420, None).encoder, Encoder::Cpu);
        clear_probe();
    }
}
