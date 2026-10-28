//! Wire payloads reconstructed from the official libstreamer ARM64 serializers.
//! See analysis/protocol-fix-20261013/datachannel for address-bound evidence.
//! These payloads do not claim server acceptance or invent missing handshake steps.

use serde_json::{json, Value};

pub const ICE_FINISHED_EVENT: &str = "ice_finished_log";
pub const FORWARD_SETTING_EVENT: &str = "forward_setting";
pub const VIDEO_CODEC_H264: i32 = 1;
pub const CODEC_IMPL_NVENC: i32 = 0;

/// Official serializer emits exactly two string fields (0x107190).
/// The event is telemetry emitted for official pc::State 6; it is not an ACK.
pub fn ice_finished(ice_id: &str, app_control_id: &str) -> Value {
    json!({"ice_id": ice_id, "app_control_id": app_control_id})
}

/// Supply the stable id of a display that this host can actually capture.
/// Official Mac display construction uses type 0 and HDR disabled value 0.
/// `codec_impl` is caller-selected: the observed names are 0 NVENC, 1 AMD AMF,
/// 2 Intel QSV, 3 VideoToolbox, 4 MediaCodec; values >=5 log as SoftWare.
/// Using 5 as software is an inference and must be runtime-validated by caller.
pub fn h264_device_capability(
    client_id: &str,
    ice_id: &str,
    display_id: i32,
    fps: i32,
    codec_impl: i32,
) -> Value {
    device_capability(client_id, ice_id, display_id, fps, codec_impl, false, false)
}

/// Shape of the official Windows host's `forward_setting` (observed from a live 4.37 host):
/// `{client_id, data: {device_capability: {display_info, ice_id, video_codec_capability}, type}}`.
/// The payload sits under `data.device_capability` with `type` beside it. The earlier flat shape
/// (`type` at the top, fields directly in `data`) was not recognised by the iPhone client, which
/// then kept quality and chroma locked at their defaults (#49). Only 8-bit and the chroma formats
/// the encoder really produces are listed, each with its maximum size: the official viewer treats a
/// 0x0 entry as unsupported (#77; the 4.37 host we copied had a GTX 660 without HEVC/4:4:4).
pub fn device_capability(client_id: &str, ice_id: &str, display_id: i32, fps: i32, codec_impl: i32, hevc: bool, full_chroma: bool) -> Value {
    let mut codecs = Vec::new();
    for codec in [VIDEO_CODEC_H264].into_iter().chain(hevc.then_some(2)) {
        for chroma in [1].into_iter().chain(full_chroma.then_some(3)) {
            let (width, height) = (3840, 2160);
            codecs.push(json!({"video_codec": codec, "chroma_sampling": chroma, "codec_impl": codec_impl,
                "bit_depth": 8, "width": width, "height": height}));
        }
    }
    json!({
        "client_id": client_id,
        "data": {
            "device_capability": {
                "display_info": [{"id": display_id, "fps": fps, "type": 0, "hdr": -1}],
                "video_codec_capability": codecs,
                "ice_id": ice_id
            },
            "type": "device_capability"
        }
    })
}

/// Official `sender_para_info` (capture and encoder names; the Windows host sends "DXGI" /
/// "NVIDIA NVENC") for the single video track.
pub fn sender_para_info(client_id: &str, ice_id: &str, capture_impl: &str, encoder_impl: &str) -> Value {
    json!({
        "client_id": client_id,
        "data": {
            "sender_para_info": {
                "capture_impl": capture_impl, "encoder_impl": encoder_impl, "ice_id": ice_id,
                "sender_media_infos": [{"capture_impl": capture_impl, "encoder_impl": encoder_impl, "video_track_index": 0}],
                "video_track_index": 0
            },
            "type": "sender_para_info"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_uses_the_official_nested_shape_and_real_formats() {
        let payload = device_capability("client-example", "ice-example", 0, 60, 5, true, true);
        assert_eq!(payload["data"]["type"], "device_capability");
        assert!(payload.get("type").is_none());
        let caps = &payload["data"]["device_capability"]["video_codec_capability"];
        assert_eq!(caps.as_array().unwrap().len(), 4);
        assert_eq!(caps[0], json!({"video_codec":1,"chroma_sampling":1,"codec_impl":5,"bit_depth":8,"width":3840,"height":2160}));
        assert_eq!(caps[3], json!({"video_codec":2,"chroma_sampling":3,"codec_impl":5,"bit_depth":8,"width":3840,"height":2160}));
        assert_eq!(payload["data"]["device_capability"]["display_info"][0]["hdr"], -1);
        let h264_420 = device_capability("c", "i", 0, 60, 5, false, false);
        assert_eq!(h264_420["data"]["device_capability"]["video_codec_capability"].as_array().unwrap().len(), 1);
        let sender = sender_para_info("c", "i", "X11", "NVIDIA NVENC");
        assert_eq!(sender["data"]["type"], "sender_para_info");
        assert_eq!(sender["data"]["sender_para_info"]["sender_media_infos"][0]["encoder_impl"], "NVIDIA NVENC");
    }

    #[test]
    fn ice_finished_keeps_string_identifiers_and_exact_fields() {
        let payload = ice_finished("ice-example", "control-example");
        assert_eq!(payload.as_object().unwrap().len(), 2);
        assert!(payload["ice_id"].is_string());
        assert_eq!(payload["app_control_id"], "control-example");
        assert_eq!(ICE_FINISHED_EVENT, "ice_finished_log");
    }

    #[test]
    fn device_capability_uses_object_data_and_string_ice_id() {
        let payload = h264_device_capability("client-example", "ice-example", 11, 60, 0);
        let capability = &payload["data"]["device_capability"];
        assert!(capability.is_object());
        assert_eq!(capability["ice_id"], "ice-example");
        let display = &capability["display_info"][0];
        assert_eq!(display.as_object().unwrap().len(), 4);
        assert_eq!(display["id"], 11);
        assert_eq!(display["fps"], 60);
        let codec = &capability["video_codec_capability"][0];
        assert_eq!(codec.as_object().unwrap().len(), 6);
        assert_eq!(codec["video_codec"], 1);
        assert_eq!(codec["codec_impl"], 0);
        assert_eq!(codec["bit_depth"], 8);
    }
}
