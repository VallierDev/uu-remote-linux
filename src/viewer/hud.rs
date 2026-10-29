//! In-window viewer chrome drawn over the remote picture, like the official client's: the loading
//! screen, the network-status panel (Ctrl+Alt+G), the control-center tab and its menu (Ctrl+Alt+M).
//! Shapes and text are rasterized on the CPU into small RGBA layers (anti-aliased); text uses a system
//! font found with fc-match (CJK capable). Without a font only shapes are drawn.
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use crate::i18n::s;

pub type Rgba = [u8; 4];
const PANEL: Rgba = [32, 34, 40, 255];
const HOVER: Rgba = [255, 255, 255, 16];
const TEXT: Rgba = [240, 242, 245, 255];
const DIM: Rgba = [160, 166, 176, 255];
const ACCENT: Rgba = [59, 130, 246, 255];
const DANGER: Rgba = [248, 113, 113, 255];

/// Straight-alpha RGBA pixels, uploaded as SDL_PIXELFORMAT_RGBA32 with alpha blending.
pub struct Layer { pub w: u32, pub h: u32, pub px: Vec<u8> }
impl Layer {
    pub fn new(w: u32, h: u32) -> Self { Self { w: w.max(1), h: h.max(1), px: vec![0; w.max(1) as usize * h.max(1) as usize * 4] } }
    fn blend(&mut self, x: i32, y: i32, c: Rgba, coverage: f32) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 || coverage <= 0. { return }
        let i = (y as usize * self.w as usize + x as usize) * 4;
        let sa = c[3] as f32 / 255. * coverage.min(1.);
        let da = self.px[i + 3] as f32 / 255.;
        let oa = sa + da * (1. - sa);
        if oa <= 0. { return }
        for k in 0..3 { self.px[i + k] = ((c[k] as f32 * sa + self.px[i + k] as f32 * da * (1. - sa)) / oa).round() as u8; }
        self.px[i + 3] = (oa * 255.).round() as u8;
    }
    /// Anti-aliased rounded rectangle; coverage from the signed distance to the rounded edge.
    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        let r = r.min(w / 2.).min(h / 2.);
        for py in y.floor() as i32..(y + h).ceil() as i32 { for px in x.floor() as i32..(x + w).ceil() as i32 {
            let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
            let qx = (fx - (x + w / 2.)).abs() - (w / 2. - r);
            let qy = (fy - (y + h / 2.)).abs() - (h / 2. - r);
            let d = (qx.max(0.).hypot(qy.max(0.))) + qx.max(qy).min(0.) - r;
            self.blend(px, py, c, 0.5 - d);
        }}
    }
    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) { self.round_rect(cx - r, cy - r, r * 2., r * 2., r, c); }
    /// Text with its top-left at (x, y); returns the advance width.
    pub fn text(&mut self, font: Option<&FontVec>, x: f32, y: f32, text: &str, size: f32, c: Rgba) -> f32 {
        let Some(font) = font else { return 0. };
        let scaled = font.as_scaled(PxScale::from(size));
        let mut caret = x;
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            let glyph = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(caret, y + scaled.ascent()));
            caret += scaled.h_advance(id);
            if let Some(outline) = font.outline_glyph(glyph) {
                let b = outline.px_bounds();
                outline.draw(|gx, gy, v| self.blend(b.min.x.floor() as i32 + gx as i32, b.min.y.floor() as i32 + gy as i32, c, v));
            }
        }
        caret - x
    }
}
pub fn measure(font: Option<&FontVec>, text: &str, size: f32) -> f32 {
    let Some(font) = font else { return text.chars().count() as f32 * size * 0.55 };
    let scaled = font.as_scaled(PxScale::from(size));
    text.chars().map(|ch| scaled.h_advance(scaled.glyph_id(ch))).sum()
}

/// The UI font: `UU_VIEWER_FONT` (a .ttf/.otf/.ttc path), else fontconfig's CJK-capable sans-serif.
pub fn load_font() -> Option<FontVec> {
    let (path, index) = match std::env::var("UU_VIEWER_FONT") {
        Ok(p) if !p.is_empty() => (p, 0),
        _ => {
            let out = std::process::Command::new("fc-match").args(["-f", "%{file}\t%{index}", "sans-serif:lang=zh-cn"]).output().ok()?;
            let line = String::from_utf8(out.stdout).ok()?;
            let (file, index) = line.split_once('\t').unwrap_or((line.as_str(), "0"));
            // fontconfig puts the variable-font named instance in the upper 16 bits; the face is below.
            (file.to_owned(), index.trim().parse::<u32>().unwrap_or(0) & 0xffff)
        }
    };
    let data = std::fs::read(&path).ok()?;
    let mut font = FontVec::try_from_vec_and_index(data, index).ok()?;
    // Variable CJK fonts default to their lightest master; use a regular UI weight.
    use ab_glyph::VariableFont;
    font.set_variation(b"wght", 450.);
    Some(font)
}

// ---------------------------------------------------------------- loading screen

pub fn spinner(t: f32, size: u32) -> Layer {
    let mut l = Layer::new(size, size);
    let c = size as f32 / 2.;
    let head = (t * 1.1).fract() * std::f32::consts::TAU;
    for i in 0..12 {
        let a = i as f32 / 12. * std::f32::consts::TAU;
        // Brightness falls off smoothly behind the rotating head.
        let behind = (head - a).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
        let alpha = (255. * (1. - behind).powf(1.6)).max(40.) as u8;
        l.circle(c + a.sin() * c * 0.72, c - a.cos() * c * 0.72, size as f32 * 0.07, [255, 255, 255, alpha]);
    }
    l
}

pub fn label_layer(font: Option<&FontVec>, text: &str, size: f32, c: Rgba) -> Layer {
    let mut l = Layer::new(measure(font, text, size).ceil() as u32 + 4, (size * 1.4).ceil() as u32);
    l.text(font, 2., 0., text, size, c);
    l
}

// ---------------------------------------------------------------- network status

#[derive(Default, Clone, PartialEq)]
pub struct NetStats { pub elapsed: u64, pub route: String, pub fps: u32, pub kbps: f64, pub rtt_ms: Option<u32>, pub loss: f64, pub size: (u32, u32), pub codec: &'static str, pub chroma: u32 }
pub fn stats_lines(n: &NetStats) -> Vec<String> {
    let rate = if n.kbps >= 1000. { format!("{:.1} Mbps", n.kbps / 1000.) } else { format!("{:.0} Kbps", n.kbps) };
    vec![
        format!("{:02}:{:02}:{:02}", n.elapsed / 3600, n.elapsed / 60 % 60, n.elapsed % 60),
        if n.route.is_empty() { "--".into() } else { n.route.clone() },
        format!("{} fps · {}", n.fps, n.codec),
        rate,
        n.rtt_ms.map_or_else(|| "RTT --".into(), |r| format!("RTT {r} ms")),
        format!("{} {:.1}%", s("丢包", "Loss"), n.loss),
        format!("{}×{} · {}", n.size.0, n.size.1, if n.chroma == 3 { "4:4:4" } else { "4:2:0" }),
    ]
}
pub fn stats_layer(font: Option<&FontVec>, lines: &[String], scale: f32) -> Layer {
    let size = 13. * scale;
    let w = lines.iter().map(|l| measure(font, l, size)).fold(0., f32::max) + 24. * scale;
    let h = lines.len() as f32 * size * 1.45 + 16. * scale;
    let mut l = Layer::new(w.ceil() as u32, h.ceil() as u32);
    l.round_rect(0., 0., w, h, 8. * scale, [0, 0, 0, 150]);
    for (i, line) in lines.iter().enumerate() { l.text(font, 12. * scale, 8. * scale + i as f32 * size * 1.45, line, size, TEXT); }
    l
}

// ---------------------------------------------------------------- control center

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Command {
    Quality(u64), Fps(u32), Chroma(u32), FollowRemote(bool), OneToOne(bool), Volume(u8),
    AutoLock(bool), HostMute(bool), GrabKeys(bool), Files, Watch(bool), Stats(bool), HideTab(bool), Fullscreen, Disconnect,
}
/// What the menu shows as selected. The host does not report remote settings back; quality, frame
/// rate and color are saved and sent again when the next session's CONTROL is ready.
#[derive(Clone, PartialEq)]
pub struct State {
    pub quality: u64, pub fps: u32, pub chroma: u32, pub follow_remote: bool, pub one_to_one: bool, pub volume: u8,
    pub auto_lock: bool, pub host_mute: bool, pub grab_keys: bool, pub watch: bool, pub stats: bool, pub hide_tab: bool, pub fullscreen: bool,
}
impl Default for State {
    fn default() -> Self { Self { quality: 5, fps: 60, chroma: 1, follow_remote: true, one_to_one: false, volume: 100, auto_lock: false,
        host_mute: false, grab_keys: false, watch: false, stats: false, hide_tab: false, fullscreen: false } }
}

enum Opt { Header(&'static str), Radio(&'static str, bool, Command), Check(&'static str, bool, Command), Action(&'static str, Command, &'static str), Slider }
fn categories() -> [&'static str; 10] {
    [s("显示屏", "Display"), s("画质", "Quality"), s("窗口", "Window"), s("声音", "Sound"), s("安全", "Security"),
     s("外设", "Peripherals"), s("工具", "Tools"), s("更多", "More"), s("全屏", "Full screen"), s("退出远控", "Disconnect")]
}
fn options(cat: usize, st: &State) -> Vec<Opt> {
    use Opt::*;
    match cat {
        0 => vec![Header(s("分辨率", "Resolution")), Radio(s("跟随被控端", "Follow the remote"), st.follow_remote, Command::FollowRemote(true)),
                  Radio(s("跟随当前窗口", "Match this window"), !st.follow_remote, Command::FollowRemote(false))],
        1 => vec![Radio(s("自动", "Auto"), st.quality == 5, Command::Quality(5)), Radio(s("原画 20M", "Original 20M"), st.quality == 4, Command::Quality(4)),
                  Radio(s("超清 8M", "Ultra HD 8M"), st.quality == 3, Command::Quality(3)), Radio(s("高清 2M", "HD 2M"), st.quality == 2, Command::Quality(2)),
                  Header(s("帧率", "Frame rate")), Radio(s("144 帧", "144 fps"), st.fps == 144, Command::Fps(144)), Radio(s("90 帧", "90 fps"), st.fps == 90, Command::Fps(90)),
                  Radio(s("60 帧", "60 fps"), st.fps == 60, Command::Fps(60)), Radio(s("30 帧", "30 fps"), st.fps == 30, Command::Fps(30)),
                  Header(s("色彩", "Color")), Radio(s("真彩 4:4:4", "True color 4:4:4"), st.chroma == 3, Command::Chroma(3)),
                  Radio(s("常规色彩", "Standard color"), st.chroma != 3, Command::Chroma(1))],
        2 => vec![Radio(s("自适应", "Fit to window"), !st.one_to_one, Command::OneToOne(false)), Radio(s("实际大小 1:1", "Actual size 1:1"), st.one_to_one, Command::OneToOne(true))],
        3 => vec![Slider],
        4 => vec![Check(s("远程结束后，被控端自动锁屏", "Lock the remote when the session ends"), st.auto_lock, Command::AutoLock(!st.auto_lock)),
                  Check(s("被控端静音运行", "Mute the remote while connected"), st.host_mute, Command::HostMute(!st.host_mute))],
        5 => vec![Header(s("键盘", "Keyboard")), Check(s("发送系统快捷键到被控端", "Send system shortcuts (Alt+Tab)"), st.grab_keys, Command::GrabKeys(!st.grab_keys))],
        6 => vec![Action(s("文件传输", "File transfer"), Command::Files, "Ctrl+Alt+F"), Check(s("观看模式", "Watch mode"), st.watch, Command::Watch(!st.watch))],
        7 => vec![Check(s("显示网络状态", "Show network status"), st.stats, Command::Stats(!st.stats)),
                  Check(s("全屏状态隐藏控制按钮", "Hide the control tab in full screen"), st.hide_tab, Command::HideTab(!st.hide_tab))],
        _ => vec![],
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Hit { Tab, Category(usize), Option(usize, usize), Slider(f32, f32), Panel }
#[derive(Clone, PartialEq)]
pub struct Menu { pub open: bool, pub category: usize, pub hover: Option<Hit>, pub dragging: bool }
impl Default for Menu { fn default() -> Self { Self { open: false, category: 1, hover: None, dragging: false } } }

pub struct Rect { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }
impl Rect { pub fn contains(&self, x: f32, y: f32) -> bool { x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h } }

pub const TAB_H: f32 = 30.;
pub fn tab_text() -> &'static str { s("控制中心", "Control center") }
/// Width of the drawn menu icon plus its gap; fonts without U+2630 would show a box.
const ICON_W: f32 = 18.;
pub fn tab_rect(font: Option<&FontVec>, window: (u32, u32), scale: f32) -> Rect {
    let w = measure(font, tab_text(), 13. * scale) + (28. + ICON_W) * scale;
    Rect { x: (window.0 as f32 - w) / 2., y: 0., w, h: TAB_H * scale }
}
pub fn tab_layer(font: Option<&FontVec>, scale: f32, hover: bool) -> Layer {
    let r = tab_rect(font, (0, 0), scale);
    let mut l = Layer::new(r.w.ceil() as u32, r.h.ceil() as u32);
    // Flat top edge: the pill hangs from the window border.
    l.round_rect(0., -12. * scale, r.w, r.h + 12. * scale, 12. * scale, if hover { [40, 44, 52, 235] } else { [24, 26, 31, 205] });
    for i in 0..3 { l.round_rect(14. * scale, r.h / 2. + (i as f32 * 4. - 5.) * scale, 11. * scale, 1.6 * scale, 0.8 * scale, TEXT); }
    l.text(font, (14. + ICON_W) * scale, (r.h - 13. * scale * 1.25) / 2., tab_text(), 13. * scale, TEXT);
    l
}

/// Menu geometry relative to its own layer; the same list drives drawing and hit testing.
/// Cascading like the official client: a narrow category list and, beside the hovered row,
/// a card sized to the selected category's options.
pub struct Layout { pub w: f32, pub h: f32, pub items: Vec<(Rect, Hit)>, pub panels: Vec<Rect> }
const ROW: f32 = 30.; const HEADER: f32 = 22.; const SLIDER: f32 = 36.; const PAD: f32 = 6.; const SHADOW: f32 = 14.;
const MAIN_W: f32 = 150.; const GAP: f32 = 6.; const TEXT_SIZE: f32 = 13.;
fn option_height(opt: &Opt) -> f32 { match opt { Opt::Header(_) => HEADER, Opt::Slider => SLIDER, _ => ROW } }
fn option_width(font: Option<&FontVec>, opt: &Opt, scale: f32) -> f32 {
    let text = |t: &str, size: f32| measure(font, t, size * scale);
    match opt {
        Opt::Header(t) => text(t, 11.) + 28. * scale,
        Opt::Radio(t, ..) | Opt::Check(t, ..) => text(t, TEXT_SIZE) + 52. * scale,
        Opt::Action(t, _, key) => text(t, TEXT_SIZE) + text(key, 11.) + 72. * scale,
        Opt::Slider => 210. * scale,
    }
}
pub fn layout(font: Option<&FontVec>, st: &State, menu: &Menu, scale: f32) -> Layout {
    let k = |v: f32| v * scale;
    let cats = categories().len();
    let main = Rect { x: k(SHADOW), y: k(SHADOW), w: k(MAIN_W), h: k(PAD * 2. + ROW * cats as f32) };
    let mut items: Vec<(Rect, Hit)> = (0..cats).map(|i| (Rect { x: main.x + k(PAD), y: main.y + k(PAD + ROW * i as f32), w: main.w - k(PAD * 2.), h: k(ROW) }, Hit::Category(i))).collect();
    let mut panels = vec![Rect { x: main.x, y: main.y, w: main.w, h: main.h }];
    let opts = options(menu.category, st);
    let (mut w, mut h) = (main.x + main.w + k(SHADOW), main.y + main.h + k(SHADOW));
    if !opts.is_empty() {
        let sub_w = opts.iter().map(|o| option_width(font, o, scale)).fold(k(180.), f32::max);
        let sub_h = k(PAD * 2.) + opts.iter().map(|o| k(option_height(o))).sum::<f32>();
        // Aligned with the selected category row, kept inside the main panel's height when possible.
        let row_y = main.y + k(PAD + ROW * menu.category as f32) - k(PAD);
        let sub = Rect { x: main.x + main.w + k(GAP), y: row_y.min((main.y + main.h - sub_h).max(main.y)), w: sub_w, h: sub_h };
        let mut y = sub.y + k(PAD);
        for (i, opt) in opts.iter().enumerate() {
            let r = Rect { x: sub.x + k(PAD), y, w: sub.w - k(PAD * 2.), h: k(option_height(opt)) };
            match opt {
                Opt::Header(_) => {}
                Opt::Slider => items.push((r, Hit::Slider(sub.x + k(18.), sub.w - k(70.)))),
                _ => items.push((r, Hit::Option(menu.category, i))),
            }
            y += k(option_height(opt));
        }
        w = sub.x + sub.w + k(SHADOW); h = h.max(sub.y + sub.h + k(SHADOW));
        panels.push(sub);
    }
    Layout { w, h, items, panels }
}
fn card(l: &mut Layer, r: &Rect, scale: f32) {
    // Soft drop shadow, then an opaque card with a hairline border.
    for i in (1..=6).rev() {
        let e = i as f32 * 2. * scale;
        l.round_rect(r.x - e / 2., r.y - e / 2. + 3. * scale, r.w + e, r.h + e, 10. * scale + e / 2., [0, 0, 0, 9]);
    }
    l.round_rect(r.x - 1., r.y - 1., r.w + 2., r.h + 2., 10. * scale + 1., [255, 255, 255, 26]);
    l.round_rect(r.x, r.y, r.w, r.h, 10. * scale, PANEL);
}
pub fn menu_layer(font: Option<&FontVec>, st: &State, menu: &Menu, scale: f32) -> (Layer, Layout) {
    let lay = layout(font, st, menu, scale);
    let k = |v: f32| v * scale;
    let mut l = Layer::new(lay.w.ceil() as u32, lay.h.ceil() as u32);
    for p in &lay.panels { card(&mut l, p, scale); }
    let size = k(TEXT_SIZE);
    let mid = |r: &Rect, size: f32| r.y + (r.h - size * 1.25) / 2.;
    let opts = options(menu.category, st);
    for (i, name) in categories().iter().enumerate() {
        let r = &lay.items[i].0;
        let selected = menu.category == i && !opts.is_empty();
        if selected { l.round_rect(r.x, r.y + 1., r.w, r.h - 2., k(6.), [255, 255, 255, 30]); }
        else if menu.hover == Some(Hit::Category(i)) { l.round_rect(r.x, r.y + 1., r.w, r.h - 2., k(6.), HOVER); }
        if i == 9 { l.round_rect(r.x + k(4.), r.y - k(1.), r.w - k(8.), 1., 0., [255, 255, 255, 22]); }
        l.text(font, r.x + k(10.), mid(r, size), name, size, if i == 9 { DANGER } else { TEXT });
        if !options(i, st).is_empty() { l.text(font, r.x + r.w - k(14.), mid(r, size), "›", size, if selected { TEXT } else { DIM }); }
    }
    if let Some(sub) = lay.panels.get(1) {
        let mut y = sub.y + k(PAD);
        for (i, opt) in opts.iter().enumerate() {
            let r = Rect { x: sub.x + k(PAD), y, w: sub.w - k(PAD * 2.), h: k(option_height(opt)) };
            if menu.hover == Some(Hit::Option(menu.category, i)) { l.round_rect(r.x, r.y + 1., r.w, r.h - 2., k(6.), HOVER); }
            match opt {
                Opt::Header(t) => { l.text(font, r.x + k(10.), r.y + k(6.), t, k(11.), DIM); }
                Opt::Radio(t, on, _) => {
                    if *on { l.circle(r.x + k(16.), r.y + r.h / 2., k(3.), ACCENT); }
                    l.text(font, r.x + k(30.), mid(&r, size), t, size, if *on { TEXT } else { [214, 218, 224, 255] });
                }
                Opt::Check(t, on, _) => {
                    // Drawn check mark: fonts without U+2713 would show a box.
                    if *on { let (cx, cy) = (r.x + k(16.), r.y + r.h / 2.);
                        for j in 0..8 { let f = j as f32 / 7.; l.circle(cx - k(4.) + f * k(3.), cy + f * k(3.), k(1.1), ACCENT); l.circle(cx - k(1.) + f * k(6.), cy + k(3.) - f * k(7.), k(1.1), ACCENT); } }
                    l.text(font, r.x + k(30.), mid(&r, size), t, size, if *on { TEXT } else { [214, 218, 224, 255] });
                }
                Opt::Action(t, _, key) => {
                    l.text(font, r.x + k(30.), mid(&r, size), t, size, TEXT);
                    let kw = measure(font, key, k(11.));
                    l.text(font, r.x + r.w - kw - k(10.), mid(&r, k(11.)), key, k(11.), DIM);
                }
                Opt::Slider => {
                    let (a, len) = (sub.x + k(18.), sub.w - k(70.));
                    let v = st.volume as f32 / 100.; let cy = r.y + r.h / 2.;
                    l.round_rect(a, cy - k(2.), len, k(4.), k(2.), [255, 255, 255, 40]);
                    l.round_rect(a, cy - k(2.), (len * v).max(k(4.)), k(4.), k(2.), ACCENT);
                    l.circle(a + len * v, cy, k(7.), [0, 0, 0, 60]);
                    l.circle(a + len * v, cy, k(6.), [255, 255, 255, 255]);
                    let label = format!("{}%", st.volume);
                    l.text(font, sub.x + sub.w - k(14.) - measure(font, &label, k(12.)), mid(&r, k(12.)), &label, k(12.), DIM);
                }
            }
            y += r.h;
        }
    }
    (l, lay)
}
/// The command for a click on an option row.
pub fn option_command(st: &State, cat: usize, index: usize) -> Option<Command> {
    match options(cat, st).get(index)? { Opt::Radio(_, _, c) | Opt::Check(_, _, c) | Opt::Action(_, c, _) => Some(*c), _ => None }
}
pub fn category_command(cat: usize) -> Option<Command> { match cat { 8 => Some(Command::Fullscreen), 9 => Some(Command::Disconnect), _ => None } }
pub fn slider_value(x: f32, a: f32, len: f32) -> u8 { (((x - a) / len).clamp(0., 1.) * 100.).round() as u8 }

/// Viewer preferences that outlive a session (`viewer.json` next to config.json).
pub fn load_prefs(st: &mut State) {
    let Ok(text) = std::fs::read_to_string(prefs_path()) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    if let Some(b) = v["stats"].as_bool() { st.stats = b; }
    if let Some(b) = v["hide_tab_fullscreen"].as_bool() { st.hide_tab = b; }
    if let Some(b) = v["one_to_one"].as_bool() { st.one_to_one = b; }
    if let Some(n) = v["volume"].as_u64() { st.volume = n.min(100) as u8; }
    if let Some(q) = v["quality"].as_u64().filter(|q| (2..=5).contains(q)) { st.quality = q; }
    if let Some(f) = v["fps"].as_u64().filter(|f| [30, 60, 90, 144].contains(f)) { st.fps = f as u32; }
    if let Some(c) = v["chroma"].as_u64().filter(|c| [1, 3].contains(c)) { st.chroma = c as u32; }
}
pub fn save_prefs(st: &State) {
    let path = prefs_path();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    let v = serde_json::json!({"stats": st.stats, "hide_tab_fullscreen": st.hide_tab, "one_to_one": st.one_to_one, "volume": st.volume,
        "quality": st.quality, "fps": st.fps, "chroma": st.chroma});
    let _ = std::fs::write(path, v.to_string());
}
fn prefs_path() -> std::path::PathBuf { crate::config::path().parent().map_or_else(|| "viewer.json".into(), |d| d.join("viewer.json")) }

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn layers_blend_inside_bounds_only(){
        let mut l=Layer::new(8,8);l.round_rect(-4.,-4.,20.,20.,3.,[255,0,0,255]);l.circle(100.,100.,5.,[0,0,0,255]);
        assert!(l.px.chunks_exact(4).all(|p|p==[255,0,0,255]));
        let mut l=Layer::new(4,4);l.round_rect(0.,0.,4.,4.,0.,[0,0,255,128]);assert_eq!(&l.px[..4],&[0,0,255,128]);
    }
    #[test] fn every_option_row_has_a_command_and_categories_map_actions(){
        let st=State::default();
        for cat in 0..10 { let lay=layout(None,&st,&Menu{category:cat,..Default::default()},1.);
            for (_,hit) in &lay.items { if let Hit::Option(c,i)=hit { assert!(option_command(&st,*c,*i).is_some()); } } }
        assert_eq!(category_command(9),Some(Command::Disconnect));assert_eq!(category_command(1),None);
        assert_eq!(slider_value(150.,100.,100.),50);assert_eq!(slider_value(0.,100.,100.),0);
    }
    /// `UU_HUD_PREVIEW=dir cargo test render_preview -- --ignored` writes the chrome as PNGs (visual check).
    #[test] #[ignore] fn render_preview(){
        let Some(dir)=std::env::var_os("UU_HUD_PREVIEW") else {return};let dir=std::path::PathBuf::from(dir);
        let font=load_font();let font=font.as_ref();
        let save=|name:&str,l:&Layer,bg:[u8;3]|{
            let rgb:Vec<u8>=l.px.chunks_exact(4).flat_map(|p|{let a=p[3] as u32;(0..3).map(move|k|((p[k] as u32*a+bg[k] as u32*(255-a))/255) as u8)}).collect();
            crate::viewer::type_ipc::write_png_rgb(&dir.join(name),l.w,l.h,&rgb,false).unwrap();
        };
        let st=State{stats:true,..State::default()};
        for cat in [1usize,3,4] {let (l,_)=menu_layer(font,&st,&Menu{open:true,category:cat,hover:Some(Hit::Option(cat,1)),dragging:false},1.);save(&format!("menu{cat}.png"),&l,[200,200,210]);}
        save("tab.png",&tab_layer(font,1.,false),[230,230,230]);
        let n=NetStats{elapsed:57,route:"UDP · relay".into(),fps:60,kbps:8420.,rtt_ms:Some(36),loss:0.,size:(1920,1080),codec:"H.265",chroma:1};
        save("stats.png",&stats_layer(font,&stats_lines(&n),1.),[230,230,230]);
        save("spinner.png",&spinner(0.3,56),[17,19,24]);
        save("title.png",&label_layer(font,"Connecting to PUNOSEVAC",20.,[240,242,245,255]),[17,19,24]);
    }
    #[test] fn stats_lines_format_numbers(){
        let n=NetStats{elapsed:3723,route:"UDP · relay".into(),fps:60,kbps:8420.,rtt_ms:Some(36),loss:0.25,size:(1920,1080),codec:"H.265",chroma:3};
        let l=stats_lines(&n);assert_eq!(l[0],"01:02:03");assert_eq!(l[3],"8.4 Mbps");assert_eq!(l[4],"RTT 36 ms");assert!(l[6].ends_with("4:4:4"));
    }
}
