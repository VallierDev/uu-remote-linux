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
        ProbeReport { chosen: choose_from_probe(&order, outcomes), outcomes: outcomes.to_vec(), order }
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

    #[test]
    fn session_before_probe_uses_legacy_auto() {
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
}
