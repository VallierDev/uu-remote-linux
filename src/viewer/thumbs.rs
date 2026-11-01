//! Device preview pictures for the launcher, like the official client's device card: the desktop
//! window saves a small PNG of the last remote picture. Local only (`$XDG_CACHE_HOME/uuremote/thumbs`,
//! 0700 directory, 0600 files); `UU_VIEWER_THUMBNAILS=0` turns it off.
use std::path::PathBuf;

pub fn enabled() -> bool { std::env::var("UU_VIEWER_THUMBNAILS").as_deref() != Ok("0") }
fn dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("uuremote").join("thumbs"))
}
pub fn path(device: &str) -> Option<PathBuf> { Some(dir()?.join(format!("{}.png", &super::type_ipc::device_key(device)[..32]))) }
pub fn existing(device: &str) -> Option<PathBuf> { enabled().then(|| path(device)).flatten().filter(|p| p.is_file()) }

/// Box-filtered downscale of the content rectangle to at most `max_w` pixels wide.
pub fn downscale(width: u32, height: u32, rgb: &[u8], content: (u32, u32, u32, u32), max_w: u32) -> (u32, u32, Vec<u8>) {
    let (cx, cy, cw, ch) = content;
    let ratio = (cw as f64 / max_w as f64).max(1.);
    let (w, h) = (((cw as f64 / ratio).round() as u32).max(1), ((ch as f64 / ratio).round() as u32).max(1));
    let mut out = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h { for x in 0..w {
        let (x0, x1) = (cx + (x as f64 * ratio) as u32, (cx + ((x + 1) as f64 * ratio) as u32).max(cx + (x as f64 * ratio) as u32 + 1).min(cx + cw).min(width));
        let (y0, y1) = (cy + (y as f64 * ratio) as u32, (cy + ((y + 1) as f64 * ratio) as u32).max(cy + (y as f64 * ratio) as u32 + 1).min(cy + ch).min(height));
        let mut sum = [0u64; 3]; let mut n = 0u64;
        for sy in y0..y1 { for sx in x0..x1 {
            let i = (sy as usize * width as usize + sx as usize) * 3;
            for k in 0..3 { sum[k] += u64::from(rgb[i + k]); } n += 1;
        }}
        for k in 0..3 { out.push((sum[k] / n.max(1)) as u8); }
    }}
    (w, h, out)
}

pub fn save(device: &str, frame: &super::desktop::Frame) -> Result<(), String> {
    if !enabled() { return Ok(()) }
    let path = path(device).ok_or("No cache directory")?;
    let dir = path.parent().ok_or("No cache directory")?;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|_| "Cannot create the thumbnail directory")?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let content = super::input::content_rect((frame.width, frame.height), frame.content.unwrap_or((frame.width, frame.height)))
        .map_or((0, 0, frame.width, frame.height), |(x, y, w, h)| (x as u32, y as u32, w as u32, h as u32));
    let (w, h, rgb) = downscale(frame.width, frame.height, &frame.rgb, content, 640);
    // Write then rename, so the launcher never reads a half-written file.
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    super::type_ipc::write_png_rgb(&tmp, w, h, &rgb, true)?;
    std::fs::rename(&tmp, &path).map_err(|_| "Cannot store the thumbnail".into())
}

#[cfg(test)] mod tests {
    #[test] fn downscale_averages_and_keeps_aspect(){
        let rgb:Vec<u8>=(0..4*2).flat_map(|i|if i%2==0{[0,0,0]}else{[200,100,50]}).collect();
        let (w,h,out)=super::downscale(4,2,&rgb,(0,0,4,2),2);
        assert_eq!((w,h),(2,1));assert_eq!(&out[..3],&[100,50,25]);
        assert_eq!(super::downscale(4,2,&rgb,(0,0,4,2),640).0,4);
    }
}
