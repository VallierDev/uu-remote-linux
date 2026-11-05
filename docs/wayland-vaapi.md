# Native Wayland VAAPI capture

The `wayland,inproc-av` build can feed native wlroots screencopy DMA-BUFs into the
existing VAAPI engine. This extends the X11/Portal implementation; it does not
replace either backend. The new path is opt-in:

```sh
UU_DESKTOP_BACKEND=wayland UU_VIDEO_ENCODER=vaapi UU_VIDEO_CODEC=h264 \
UU_WAYLAND_DMABUF=1 UU_WAYLAND_VAAPI_INPROC=1 uuremote-server
```

Run inside the graphical user session with access to the VAAPI render node.
`UU_VAAPI_DEVICE` selects the encoder device. `UU_WAYLAND_DMABUF_DEVICE` selects the
GBM allocation device; both should refer to a compatible GPU. No permissions are
changed automatically. A build with `engine-plugins` needs the matching Wayland
plugin from the same build.

The compositor writes a single-plane linear RGB DMA-BUF. VAAPI imports it, VPP
converts/scales it and the hardware encodes H.264/HEVC. The capture lease remains
locked until VA completion; only the compressed packet is copied to host memory.
There is no raw-video pipe, CPU pixel mapping or FFmpeg video subprocess on this
path. This is zero **CPU pixel copying**, not a promise that the compositor/VPP
never performs GPU copies. Startup still checks FFmpeg for other paths and tools.

Confirm `pipeline=in-process` followed by
`Native Wayland VAAPI pipeline=dma-buf zero_cpu_copy=true`. Unsupported layouts,
rotated/flipped outputs, missing DMA-BUF support or driver errors leave the
existing FFmpeg fallback available. The initial implementation accepts normal
output transforms and single-plane linear RGB only. GPU format/modifier support
and hybrid-GPU operation require their own validation. The native engine shares
one connection/buffer pair, so only one stream holds the DMA-BUF session at a
time; additional streams use the existing fallback instead of reusing a live
encoder surface.

## Damage-driven pacing

After the first picture, `copy_with_damage` waits for output changes. The viewer's
FPS setting is an upper bound. A static output sends approximately one keepalive
picture per second; IDR requests can encode the last imported picture sooner.
The alternate buffer remains pending, so an idle keepalive does not overwrite a
compositor-owned capture buffer. Capture checks readiness without blocking under
the input mutex; retries are paced in Rust outside that mutex. Pending protocol
work retries independently of the encoded-frame FPS deadline, with a short retry
and a 10 ms idle backoff. A cancelled encoder
releases its pending request before the next stream starts. Current-frame RTP
timestamps follow encoder completion time, including idle gaps and resumed motion.
Distinct pictures advance by at least one 90 kHz tick even when FFmpeg splits
several pictures from one read. Locked-desktop pictures use the same clock.

`UU_WAYLAND_DYNAMIC_FPS=0` restores fixed-rate capture for comparison. This option
affects only native Wayland VAAPI. It does not change Portal or CPU encoding.

## Verification

Hardware-free checks:

```sh
cargo test --locked
cargo test --locked --features wayland,inproc-av
sh tests/run-wayland-dmabuf-tests.sh
```

The native C test uses controlled protocol/GBM endpoints and executes the actual
capture code under ASan/UBSan. It checks lease ordering, consumer failure, timeout
followed by readiness, damage request selection, unsupported layouts and cleanup.
Rust tests cover FFI error/panic containment, idle/IDR pacing and RTP timestamps.

On an unlocked wlroots desktop, with a static test scene, build and run the
ignored hardware regression:

```sh
UU_WAYLAND_DMABUF=1 UU_WAYLAND_DYNAMIC_FPS=1 cargo test --release --locked --features wayland,inproc-av \
  wayland_dma_idle_idr_and_restarts -- --ignored --nocapture --test-threads=1
```

It runs ten native capture/encode cycles, checks first-frame IDRs, requests an IDR
while idle, bounds shutdown and checks file-descriptor growth. It does not inject
keyboard input or read account credentials. A changing desktop can prevent the
idle precondition; use an isolated static scene.

Development measurements on Intel i3-8145U/UHD 620, 1080p H.264, target 30 fps,
2 Mbps: one 24-second probe session transitioned idle → animated → idle, with
probe-process CPU of 2.12%, 3.62%, and 0.25% (100% = one logical CPU). A five-second
active window encoded 151 changed pictures. All 273 output pictures decoded;
263 were distinct. The probe was intentionally terminated after the timed phases.
These figures exclude audio, network, compositor and scene-generator work. They
are observations from the development build, not equivalent-content benchmarks
or guarantees for other hardware. Earlier fixed-rate FFmpeg/native comparisons
had different distinct-picture counts and are not used to claim a speedup.

Full-client lock/unlock, multi-monitor switching, HEVC, long-running resource
behavior and non-Intel drivers are not covered by that measurement.
