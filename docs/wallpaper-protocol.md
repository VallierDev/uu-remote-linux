# Device-card wallpaper reporting

The official Windows host publishes the system wallpaper for device cards. This is separate from
the Linux viewer's local last-frame cache and does not require capturing desktop windows on disconnect.

## Static protocol evidence

Read-only analysis of the locally installed `GameViewerServer.exe` (SHA-256
`af3a15c3887e06bfa9506971e01cd8ec58f41efe8ac9eb5831e31dae1fc1cef8`) found:

- `ApplicationManager::updateWallpaper`, `getWallpaperPath`, `uploadWallpaper`, and log strings
  `wallpaper check`, `wallpaper is not changed, the same md5`, `wallpaper file upload succeeded`.
  The source path recorded in the executable is `server_core/business/apps/application_manager.cpp`.
- `0x140ade430`: GET `/api/v1/tool/fp/token?filetype=1`. Response parser `0x140ae3410`
  reads `token`, `req_url`, `ttl`, optional `max_size` from API data.
- `0x140aeb8c0`: POST raw file bytes to `req_url`, with `Authorization` set directly to the
  file token (no Bearer prefix, multipart wrapper, or account credentials).
- Upload callback `0x140ad1010` requires HTTP 200; parser `0x140ae1550` reads top-level
  `fsize`, `url`, `md5`, `mime`. The wallpaper callback `0x1404206c0` uses `url`.
- `0x140b48d80`: POST `/api/v1/device/wallpaper`, JSON `{"wallpaper_url":"<uploaded URL>"}`.
  Device response parsers and the official viewer also contain `wallpaper_url`.

Addresses are preferred virtual addresses for that executable only. No official binary, assets,
account data, upload tokens, desktop pictures, or account URLs are included here.
This establishes the wallpaper flow statically; it does not prove that every official client's
last-frame display uses this flow or establish the official timer interval.

## Linux behavior

Signed-in graphical hosts check the configured wallpaper on startup and every 60 seconds, upload
only changed supported image bytes, and retry failures on a later check. Guest and headless hosts skip
reporting. Account renewal replaces the reporting task; stopping the host cancels it. Network work
does not block signaling or alter peer connections. Images are bounded to 10 MiB and the file service's
advertised limit. HTTPS is required and upload redirects are disabled.

GNOME/Unity/Cinnamon use GSettings (GNOME dark preference included); KDE reads the first image
wallpaper containment, including image packages, or the installed look-and-feel's default wallpaper
when no explicit wallpaper is configured; Xfce reads the first `last-image`. Multi-monitor selection, slideshow/XML
wallpapers and unsupported formats are not inferred.
On Hyprland, Omarchy's `omarchy/current/background` link is read from `XDG_STATE_HOME`
(default `~/.local/state`), falling back to the older `XDG_CONFIG_HOME` location
(default `~/.config`). Wallpaper/theme changes are followed on the next check.
Other compositor-specific wallpaper daemons are not inferred.
`UU_WALLPAPER=/absolute/path/image.png` selects a file explicitly; `UU_WALLPAPER_UPLOAD=0` disables
reporting. PNG, JPEG, and WebP are supported. No desktop screenshot fallback is used.

Only success/failure is logged. Paths, hashes, URLs, response bodies and credentials stay out of logs.
