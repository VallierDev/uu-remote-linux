//! Translate UU's verified Windows-VK input path into X11 KeySyms.
//! A KeySym is not an X11 keycode: the input engine must resolve it against the
//! active X keyboard mapping (e.g. XKeysymToKeycode) before XTest injection.
//! Letters use the unshifted lowercase symbol; separate modifier events carry
//! Shift/Ctrl/Alt state. Unknown VKs are ignored instead of injecting raw values.

/// Map an official Windows virtual-key identifier to a base X11 KeySym.
/// The wire has no extended-key bit; VK_RETURN consequently means Return.
/// OEM punctuation follows the US base-symbol interpretation; other physical
/// layouts may need their own OEM policy. OEM_102 is deliberately unsupported.
pub fn windows_vk_to_keysym(vk: i32) -> Option<u64> {
    let keysym = match vk {
        0x30..=0x39 => vk as u64,                // 0..9
        0x41..=0x5a => (vk + 0x20) as u64,      // A..Z -> a..z
        0x60..=0x69 => 0xffb0 + (vk - 0x60) as u64, // KP_0..KP_9
        0x70..=0x87 => 0xffbe + (vk - 0x70) as u64, // F1..F24
        0x08 => 0xff08, // BackSpace
        0x09 => 0xff09, // Tab
        0x0c => 0xff0b, // Clear
        0x0d => 0xff0d, // Return
        0x10 | 0xa0 => 0xffe1, // Shift / Shift_L
        0xa1 => 0xffe2, // Shift_R
        0x11 | 0xa2 => 0xffe3, // Control / Control_L
        0xa3 => 0xffe4, // Control_R
        0x12 | 0xa4 => 0xffe9, // Alt / Alt_L
        0xa5 => 0xffea, // Alt_R
        0x13 => 0xff13, // Pause
        0x14 => 0xffe5, // Caps_Lock
        0x1b => 0xff1b, // Escape
        0x20 => 0x0020, // space
        0x21 => 0xff55, // Prior / Page_Up
        0x22 => 0xff56, // Next / Page_Down
        0x23 => 0xff57, // End
        0x24 => 0xff50, // Home
        0x25 => 0xff51, // Left
        0x26 => 0xff52, // Up
        0x27 => 0xff53, // Right
        0x28 => 0xff54, // Down
        0x2c => 0xff61, // Print
        0x2d => 0xff63, // Insert
        0x2e => 0xffff, // Delete
        0x5b => 0xffeb, // Super_L
        0x5c => 0xffec, // Super_R
        0x5d => 0xff67, // Menu
        0x6a => 0xffaa, // KP_Multiply
        0x6b => 0xffab, // KP_Add
        0x6c => 0xffac, // KP_Separator
        0x6d => 0xffad, // KP_Subtract
        0x6e => 0xffae, // KP_Decimal
        0x6f => 0xffaf, // KP_Divide
        0x90 => 0xff7f, // Num_Lock
        0x91 => 0xff14, // Scroll_Lock
        0xba => 0x003b, // OEM_1: semicolon
        0xbb => 0x003d, // OEM_PLUS: equal
        0xbc => 0x002c, // OEM_COMMA
        0xbd => 0x002d, // OEM_MINUS
        0xbe => 0x002e, // OEM_PERIOD
        0xbf => 0x002f, // OEM_2: slash
        0xc0 => 0x0060, // OEM_3: grave
        0xdb => 0x005b, // OEM_4: bracketleft
        0xdc => 0x005c, // OEM_5: backslash
        0xdd => 0x005d, // OEM_6: bracketright
        0xde => 0x0027, // OEM_7: apostrophe
        // OEM_102 depends on the physical keyboard/layout; do not guess.
        _ => return None,
    };
    Some(keysym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_shift_and_left_use_named_keysyms() {
        assert_eq!(windows_vk_to_keysym(160), Some(0xffe1));
        assert_eq!(windows_vk_to_keysym(37), Some(0xff51));
        assert_ne!(windows_vk_to_keysym(160), Some(160));
    }

    #[test]
    fn letters_use_unshifted_symbols_and_digits_keep_ascii() {
        assert_eq!(windows_vk_to_keysym(65), Some(u64::from(b'a')));
        assert_eq!(windows_vk_to_keysym(90), Some(u64::from(b'z')));
        for digit in b'0'..=b'9' {
            assert_eq!(windows_vk_to_keysym(i32::from(digit)), Some(u64::from(digit)));
        }
    }

    #[test]
    fn modifiers_preserve_left_and_right_identity() {
        let variants = [
            (0x10, 0xffe1), (0xa0, 0xffe1), (0xa1, 0xffe2),
            (0x11, 0xffe3), (0xa2, 0xffe3), (0xa3, 0xffe4),
            (0x12, 0xffe9), (0xa4, 0xffe9), (0xa5, 0xffea),
            (0x5b, 0xffeb), (0x5c, 0xffec),
        ];
        for (vk, expected) in variants {
            assert_eq!(windows_vk_to_keysym(vk), Some(expected));
        }
    }

    #[test]
    fn function_and_keypad_ranges_have_correct_endpoints() {
        assert_eq!(windows_vk_to_keysym(112), Some(0xffbe));
        assert_eq!(windows_vk_to_keysym(135), Some(0xffd5));
        assert_eq!(windows_vk_to_keysym(96), Some(0xffb0));
        assert_eq!(windows_vk_to_keysym(105), Some(0xffb9));
        assert_eq!(windows_vk_to_keysym(106), Some(0xffaa));
        assert_eq!(windows_vk_to_keysym(111), Some(0xffaf));
    }

    #[test]
    fn navigation_and_oem_base_keys_are_not_raw_keycodes() {
        assert_eq!(windows_vk_to_keysym(46), Some(0xffff));
        assert_eq!(windows_vk_to_keysym(36), Some(0xff50));
        assert_eq!(windows_vk_to_keysym(186), Some(u64::from(b';')));
        assert_eq!(windows_vk_to_keysym(187), Some(u64::from(b'=')));
        assert_eq!(windows_vk_to_keysym(220), Some(u64::from(b'\\')));
        assert_eq!(windows_vk_to_keysym(222), Some(u64::from(b'\'')));
    }

    #[test]
    fn unknown_or_layout_specific_keys_are_ignored() {
        for vk in [-1, 0, 7, 21, 136, 226, 255, 256, i32::MAX] {
            assert_eq!(windows_vk_to_keysym(vk), None);
        }
    }
}

/// Official Mac CONTROL absolute coordinates are normalized to the source screen.
pub fn normalized_pointer(x: f64, y: f64, width: u32, height: u32) -> Option<(i32, i32)> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32
        || !x.is_finite() || !y.is_finite() || !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) { return None; }
    Some(((x * (width - 1) as f64).round() as i32, (y * (height - 1) as f64).round() as i32))
}

#[cfg(test)]
mod pointer_tests {
    use super::normalized_pointer;
    #[test]
    fn maps_source_screen_edges_and_center() {
        assert_eq!(normalized_pointer(0.0, 0.0, 1920, 1080), Some((0,0)));
        assert_eq!(normalized_pointer(1.0, 1.0, 1920, 1080), Some((1919,1079)));
        assert_eq!(normalized_pointer(0.5, 0.5, 1920, 1080), Some((960,540)));
    }
    #[test]
    fn maps_observed_editor_titlebar_coordinate() {
        assert_eq!(normalized_pointer(0.6805736636245111, 0.15990730011587487,1920,1080),Some((1306,173)));
    }
    #[test]
    fn rejects_invalid_coordinate_or_screen_dimensions() {
        assert_eq!(normalized_pointer(f64::NAN,0.5,1920,1080),None);
        assert_eq!(normalized_pointer(0.5,f64::INFINITY,1920,1080),None);
        assert_eq!(normalized_pointer(-0.1,0.5,1920,1080),None);
        assert_eq!(normalized_pointer(0.5,1.1,1920,1080),None);
        assert_eq!(normalized_pointer(0.5,0.5,0,1080),None);
    }
}

/// Official clients send Windows mouse-key bitmasks, not X11 button ordinals: MK_LBUTTON 1,
/// MK_RBUTTON 2, MK_MBUTTON 16, MK_XBUTTON1 32, MK_XBUTTON2 64. An official client's middle
/// button arrives as 16 (it used to log "Unsupported desktop wire mouse button: 16" and be
/// dropped); 32 and 64 follow the same mask but have not been observed yet. 4 is what older
/// builds of this project's own viewer sent for the middle button, kept for compatibility.
pub fn mouse_button_to_x11(button: i32) -> Option<i32> {
    match button { 1 => Some(1), 2 => Some(3), 4 | 16 => Some(2), 32 => Some(8), 64 => Some(9), _ => None }
}
#[cfg(test)]
mod button_tests {
    #[test]
    fn maps_verified_button_bitmasks() {
        assert_eq!(super::mouse_button_to_x11(1),Some(1));
        assert_eq!(super::mouse_button_to_x11(2),Some(3));
        assert_eq!(super::mouse_button_to_x11(16),Some(2));
        assert_eq!(super::mouse_button_to_x11(4),Some(2));
        assert_eq!(super::mouse_button_to_x11(32),Some(8));
        assert_eq!(super::mouse_button_to_x11(64),Some(9));
        assert_eq!(super::mouse_button_to_x11(3),None);
    }
}
