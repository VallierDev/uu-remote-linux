//! Runtime encoder / codec preference. host_settings.json beats the environment,
//! and "auto" or a missing field falls through. Illegal values are ignored.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::capture_setting::{CHROMA_420, CHROMA_444};
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub order: Vec<Encoder>,
    pub ok: Vec<bool>,
    pub chosen: Encoder,
}

fn cache() -> &'static Mutex<Option<ProbeReport>> {
    static CACHE: OnceLock<Mutex<Option<ProbeReport>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// SIGUSR2 bumps this. Each desktop stream keeps the generation it already applied.
pub fn reload_generation() -> &'static AtomicU64 {
    static GEN: AtomicU64 = AtomicU64::new(0);
    &GEN
}

pub fn note_reload() {
    reload_generation().fetch_add(1, Ordering::Release);
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

fn file_field(name: &str) -> Option<String> {
    let path = crate::system_actions::settings_path();
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get(name).and_then(|v| v.as_str()).map(str::to_owned)
}

fn legacy_auto(nvidia: bool) -> Encoder {
    if nvidia {
        Encoder::Nvenc
    } else {
        Encoder::Cpu
    }
}

/// Explicit file or env keeps the v0.2.23 rule. Auto uses the cached probe, else the old NVIDIA rule.
pub fn selection() -> Selection {
    let file_enc = file_field("video_encoder");
    let file_codec = file_field("video_codec");
    let env_enc = std::env::var("UU_VIDEO_ENCODER").ok();
    let env_codec = std::env::var("UU_VIDEO_CODEC").ok();
    let probed = cache().lock().ok().and_then(|g| g.clone());
    resolve_pair(
        file_enc.as_deref(),
        env_enc.as_deref(),
        file_codec.as_deref(),
        env_codec.as_deref(),
        crate::video::nvidia_present(),
        false,
        probed.as_ref(),
    )
}

pub fn resolve_pair(
    enc_setting: Option<&str>,
    enc_env: Option<&str>,
    codec_setting: Option<&str>,
    codec_env: Option<&str>,
    nvidia: bool,
    _display_nvidia: bool,
    probed: Option<&ProbeReport>,
) -> Selection {
    let (encoder, encoder_source) = match parse_encoder(enc_setting) {
        Some(Some(e)) => (e, Source::Setting),
        None => {
            log::warn!("Ignoring video_encoder value (not auto/cpu/vaapi/nvenc)");
            pick_encoder(None, enc_env, nvidia, probed)
        }
        Some(None) => pick_encoder(None, enc_env, nvidia, probed),
    };
    let (codec_pref, codec_source) = match parse_codec(codec_setting) {
        Some(Some(p)) => (p, Source::Setting),
        None => {
            log::warn!("Ignoring video_codec value (not auto/h264/h265)");
            pick_codec(None, codec_env)
        }
        Some(None) => pick_codec(None, codec_env),
    };
    Selection {
        encoder,
        encoder_source,
        codec_pref,
        codec_source,
    }
}

fn pick_encoder(
    _explicit: Option<Encoder>,
    env: Option<&str>,
    nvidia: bool,
    probed: Option<&ProbeReport>,
) -> (Encoder, Source) {
    match parse_encoder(env) {
        Some(Some(e)) => (e, Source::Env),
        None => {
            log::warn!("Ignoring UU_VIDEO_ENCODER value (not auto/cpu/vaapi/nvenc)");
            auto_encoder(nvidia, probed)
        }
        Some(None) => auto_encoder(nvidia, probed),
    }
}

fn auto_encoder(nvidia: bool, probed: Option<&ProbeReport>) -> (Encoder, Source) {
    (
        probed
            .map(|p| p.chosen)
            .unwrap_or_else(|| legacy_auto(nvidia)),
        Source::Auto,
    )
}

fn pick_codec(_explicit: Option<CodecPref>, env: Option<&str>) -> (CodecPref, Source) {
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

/// 4:4:4 is NVENC and libx264. VAAPI stays 4:2:0.
pub fn encoder_full_chroma(encoder: Encoder) -> bool {
    encoder != Encoder::Vaapi
}

/// Auto + 真彩 skips VAAPI for the next candidate that can do 4:4:4.
/// An explicit `vaapi` setting stays on VAAPI; the stream then drops to 4:2:0.
pub fn encoder_for_chroma(
    selection: Selection,
    chroma: u64,
    probed: Option<&ProbeReport>,
) -> Encoder {
    if chroma != CHROMA_444 || encoder_full_chroma(selection.encoder) {
        return selection.encoder;
    }
    if selection.encoder_source != Source::Auto {
        return selection.encoder;
    }
    // Only a candidate that produced an IDR replaces VAAPI. Without a probe, NVENC then CPU.
    if let Some(report) = probed {
        return report
            .order
            .iter()
            .zip(report.ok.iter())
            .find_map(|(e, good)| {
                (*good && *e != Encoder::Vaapi && encoder_full_chroma(*e)).then_some(*e)
            })
            .unwrap_or(Encoder::Cpu);
    }
    Encoder::Nvenc
}

pub fn chroma_after_switch(encoder: Encoder, chroma: u64) -> u64 {
    if chroma == CHROMA_444 && !encoder_full_chroma(encoder) {
        CHROMA_420
    } else {
        chroma
    }
}

/// Candidate order for one probe. Display on NVIDIA tries NVENC first.
pub fn candidate_order(display_nvidia: bool, nvidia: bool) -> Vec<Encoder> {
    if display_nvidia {
        vec![Encoder::Nvenc, Encoder::Cpu]
    } else {
        let mut order = vec![Encoder::Vaapi];
        if nvidia {
            order.push(Encoder::Nvenc);
        }
        order.push(Encoder::Cpu);
        order
    }
}

pub fn choose_from_probe(order: &[Encoder], ok: &[bool]) -> Encoder {
    order
        .iter()
        .zip(ok.iter())
        .find_map(|(e, good)| good.then_some(*e))
        .unwrap_or(Encoder::Cpu)
}

pub fn store_probe(report: ProbeReport) {
    if let Ok(mut slot) = cache().lock() {
        *slot = Some(report);
    }
}

pub fn cached_probe() -> Option<ProbeReport> {
    cache().lock().ok().and_then(|g| g.clone())
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
        probe: Option<&ProbeReport>,
    ) -> Selection {
        resolve_pair(es, ee, cs, ce, nvidia, false, probe)
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
                let got = sel(setting, env, None, None, true, None);
                let expect_enc = match (parse_encoder(setting), parse_encoder(env)) {
                    (Some(Some(e)), _) => (e, Source::Setting),
                    (None, Some(Some(e))) => (e, Source::Env),
                    (Some(None), Some(Some(e))) => (e, Source::Env),
                    (None, None)
                    | (None, Some(None))
                    | (Some(None), None)
                    | (Some(None), Some(None)) => (Encoder::Nvenc, Source::Auto),
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
                let got = sel(None, None, setting, env, false, None);
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
            sel(
                Some("auto"),
                Some("cpu"),
                Some("auto"),
                Some("h264"),
                true,
                None
            )
            .encoder,
            Encoder::Cpu
        );
        assert_eq!(
            sel(Some("auto"), None, None, None, false, None).encoder,
            Encoder::Cpu
        );
        assert_eq!(
            sel(None, Some("auto"), None, None, true, None).encoder,
            Encoder::Nvenc
        );
    }

    #[test]
    fn auto_uses_injected_probe_order() {
        let order = candidate_order(false, true);
        assert_eq!(order, vec![Encoder::Vaapi, Encoder::Nvenc, Encoder::Cpu]);
        assert_eq!(
            candidate_order(true, true),
            vec![Encoder::Nvenc, Encoder::Cpu]
        );
        assert_eq!(
            candidate_order(false, false),
            vec![Encoder::Vaapi, Encoder::Cpu]
        );
        let report = ProbeReport {
            order: order.clone(),
            ok: vec![true, true, true],
            chosen: choose_from_probe(&order, &[true, true, true]),
        };
        assert_eq!(report.chosen, Encoder::Vaapi);
        let got = sel(None, None, None, None, true, Some(&report));
        assert_eq!(
            (got.encoder, got.encoder_source),
            (Encoder::Vaapi, Source::Auto)
        );
        let failed = ProbeReport {
            chosen: choose_from_probe(&order, &[false, true, true]),
            ok: vec![false, true, true],
            order: order.clone(),
        };
        assert_eq!(failed.chosen, Encoder::Nvenc);
        assert_eq!(
            sel(Some("cpu"), None, None, None, true, Some(&report)).encoder,
            Encoder::Cpu
        );
    }

    #[test]
    fn vaapi_444_uses_next_capable_candidate() {
        let order = vec![Encoder::Vaapi, Encoder::Nvenc, Encoder::Cpu];
        let report = ProbeReport {
            order: order.clone(),
            ok: vec![true, true, true],
            chosen: Encoder::Vaapi,
        };
        let auto = sel(None, None, None, None, true, Some(&report));
        assert_eq!(
            encoder_for_chroma(auto, CHROMA_444, Some(&report)),
            Encoder::Nvenc
        );
        let nvenc_down = ProbeReport {
            ok: vec![true, false, true],
            chosen: Encoder::Vaapi,
            order,
        };
        assert_eq!(
            encoder_for_chroma(auto, CHROMA_444, Some(&nvenc_down)),
            Encoder::Cpu
        );
        assert_eq!(
            encoder_for_chroma(auto, CHROMA_420, Some(&report)),
            Encoder::Vaapi
        );
        let pinned = sel(Some("vaapi"), None, None, None, true, Some(&report));
        assert_eq!(
            encoder_for_chroma(pinned, CHROMA_444, Some(&report)),
            Encoder::Vaapi
        );
        assert_eq!(chroma_after_switch(Encoder::Vaapi, CHROMA_444), CHROMA_420);
        assert_eq!(chroma_after_switch(Encoder::Nvenc, CHROMA_444), CHROMA_444);
    }
}
