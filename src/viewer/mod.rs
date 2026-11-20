//! Opt-in controller session; independent of the host room and peer lifecycle.
pub mod api;
pub mod assist;
pub mod wire;
pub mod signal;
pub mod rtc;
pub mod session;
#[cfg(test)] mod tests;

pub mod media;
pub mod audio;
pub mod input;
pub mod desktop;
#[cfg(feature="viewer-ffmpeg")] pub mod decoder;
#[cfg(any(test,feature="viewer-sdl"))] pub mod decoder_cli;
#[cfg(uu_gtk)] pub mod ui;
#[cfg(uu_gtk)] pub mod tools_ui;
#[cfg(feature="viewer-sdl")] pub mod sdl;
#[cfg(feature="viewer-sdl")] pub mod hud;
pub mod thumbs;
#[cfg(uu_gtk)] pub mod host;

pub mod tools;
pub mod native_gamepad;
pub mod native_rumble;

pub mod clipboard;
pub mod cursor;
pub mod power;
pub mod secure_input;

/// Why a desktop session ended without a picture, so the caller can log the reason
/// and retry. The host answered with a session that carries no key frame: the media
/// channel works but the decoder can never start, so the screen stays black.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum NoPicture {
    /// The CONTROL data channel never opened, so the echo handshake and the
    /// sendVideoTrack RPC never happened and the host cannot show a picture.
    ControlNotOpen,
    /// CONTROL is open but no frame was ever presented.
    NoKeyFrame,
}
impl NoPicture {
    /// Short reason for the retry log.
    pub fn reason(self)->&'static str { match self {
        NoPicture::ControlNotOpen=>"control channel not open",
        NoPicture::NoKeyFrame=>"no key frame",
    }}
}
pub mod type_ipc;

fn device_label(name:Option<&str>)->&str{
    name.filter(|name|!name.is_empty()).unwrap_or_else(||crate::i18n::s("远端设备","Remote device"))
}

#[cfg(test)]
mod device_label_tests {
    #[test]
    fn device_name_or_fallback(){
        assert_eq!(super::device_label(Some("测试电脑")),"测试电脑");
        assert_eq!(super::device_label(None),crate::i18n::s("远端设备","Remote device"));
        assert_eq!(super::device_label(Some("")),crate::i18n::s("远端设备","Remote device"));
    }
}
