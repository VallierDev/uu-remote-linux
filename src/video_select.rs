//! Runtime encoder / codec preference. host_settings.json beats the environment,
//! and "auto" or a missing field falls through. Illegal values are ignored.
//! Auto keeps the v0.2.23 rule (NVENC when an NVIDIA driver is present, otherwise CPU).

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
fn legacy_auto(nvidia: bool) -> Encoder {
    if nvidia { Encoder::Nvenc } else { Encoder::Cpu }
}

/// Setting (non-auto) beats the environment, which beats the NVIDIA/CPU default.
pub fn selection() -> Selection {
    let (file_enc, file_codec) = file_fields();
    let env_enc = std::env::var("UU_VIDEO_ENCODER").ok();
    let env_codec = std::env::var("UU_VIDEO_CODEC").ok();
    resolve_pair(
        file_enc.as_deref(),
        env_enc.as_deref(),
        file_codec.as_deref(),
        env_codec.as_deref(),
        crate::video::nvidia_present(),
    )
}

pub fn resolve_pair(
    enc_setting: Option<&str>,
    enc_env: Option<&str>,
    codec_setting: Option<&str>,
    codec_env: Option<&str>,
    nvidia: bool,
) -> Selection {
    let (encoder, encoder_source) = match parse_encoder(enc_setting) {
        Some(Some(e)) => (e, Source::Setting),
        None => {
            log::warn!("Ignoring video_encoder value (not auto/cpu/vaapi/nvenc)");
            pick_encoder(enc_env, nvidia)
        }
        Some(None) => pick_encoder(enc_env, nvidia),
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

fn pick_encoder(env: Option<&str>, nvidia: bool) -> (Encoder, Source) {
    match parse_encoder(env) {
        Some(Some(e)) => (e, Source::Env),
        None => {
            log::warn!("Ignoring UU_VIDEO_ENCODER value (not auto/cpu/vaapi/nvenc)");
            (legacy_auto(nvidia), Source::Auto)
        }
        Some(None) => (legacy_auto(nvidia), Source::Auto),
    }
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
        resolve_pair(es, ee, cs, ce, nvidia)
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
}
