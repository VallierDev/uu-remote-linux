//! UU capture-source metadata used by the official streaming core.
//! URI and big-endian uint16 layout are verified in the official ARM64 library.
//! Source index 0 is this implementation's sole primary capture source.
use webrtc::rtp::extension::HeaderExtension;
use webrtc::util::{Marshal, MarshalSize};

pub const CAPTURE_INDEX_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index";
pub const NEW_FRAME_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame";

pub struct NewFrame(pub bool);
impl MarshalSize for NewFrame { fn marshal_size(&self) -> usize { 1 } }
impl Marshal for NewFrame {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let byte = buffer.first_mut().ok_or_else(|| webrtc::util::Error::Other("New frame buffer too short".into()))?;
        *byte = u8::from(self.0);
        Ok(1)
    }
}
pub fn new_frame() -> HeaderExtension {
    is_new_frame(true)
}
pub fn is_new_frame(is_new: bool) -> HeaderExtension {
    HeaderExtension::Custom { uri: NEW_FRAME_URI.into(), extension: Box::new(NewFrame(is_new)) }
}

#[derive(Debug)]
pub struct CaptureIndex(pub u16);

impl MarshalSize for CaptureIndex {
    fn marshal_size(&self) -> usize { 2 }
}

impl Marshal for CaptureIndex {
    fn marshal_to(&self, buffer: &mut [u8]) -> webrtc::util::Result<usize> {
        let target = buffer.get_mut(..2)
            .ok_or_else(|| webrtc::util::Error::Other("Capture index buffer too short".into()))?;
        target.copy_from_slice(&self.0.to_be_bytes());
        Ok(2)
    }
}

pub fn capture_index(index: u16) -> HeaderExtension {
    HeaderExtension::Custom {
        uri: CAPTURE_INDEX_URI.into(), extension: Box::new(CaptureIndex(index)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encodes_network_order_without_truncation() {
        let mut bytes = [0u8; 2];
        assert_eq!(CaptureIndex(0x1234).marshal_to(&mut bytes).unwrap(), 2);
        assert_eq!(bytes, [0x12, 0x34]);
        assert!(CaptureIndex(1).marshal_to(&mut [0u8; 1]).is_err());
    }
    #[test]
    fn new_frame_is_one_byte_boolean() {
        let mut bytes = [0];
        NewFrame(true).marshal_to(&mut bytes).unwrap();
        assert_eq!(bytes, [1]);
        NewFrame(false).marshal_to(&mut bytes).unwrap();
        assert_eq!(bytes, [0]);
    }
}
