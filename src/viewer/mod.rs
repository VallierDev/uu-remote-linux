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

pub mod clipboard;
pub mod power;
pub mod secure_input;
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
