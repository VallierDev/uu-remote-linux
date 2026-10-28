//! Standalone benchmark tool measuring:
//! 1. Host Memory Copy Bandwidth & CPU Cost (What FFmpeg/Sway CPU swizzling does)
//! 2. NVENC H.265 Hardware Throughput & Latency (1920x1200 @ ultra-low-latency)
//! 3. NVENC reading a registered (pinned) host buffer

use std::time::Instant;

fn main() {
    println!("=========================================================");
    println!("   UU Remote Linux: Encoder Pipeline Benchmark Suite     ");
    println!("=========================================================");
    println!();

    let width: u32 = 1920;
    let height: u32 = 1200;
    let frames: usize = 300; // 5 seconds of 60fps video
    let frame_bytes: usize = (width * height * 4) as usize;

    println!("Test Configuration:");
    println!("  Resolution : {}x{}", width, height);
    println!("  Frame Size : {:.2} MB (ARGB32)", frame_bytes as f64 / 1024.0 / 1024.0);
    println!("  Iterations : {} frames (Equivalent to 5.0s @ 60 FPS)", frames);
    println!();

    // Simulating XShm framebuffer
    let mut host_pixels = vec![0x55u8; frame_bytes];

    // Benchmark 1: CPU Memcpy (External FFmpeg stdin / Sway wlr-screencopy CPU copy baseline)
    println!("[1] Baseline: CPU Frame Memcpy Overhead (IPC / Process Pipe Cost)");
    let mut sink = vec![0u8; frame_bytes];
    let start_memcpy = Instant::now();
    for _ in 0..frames {
        sink.copy_from_slice(&host_pixels);
        std::hint::black_box(&sink);
    }
    let duration_memcpy = start_memcpy.elapsed();
    let memcpy_per_frame = duration_memcpy.as_secs_f64() * 1000.0 / frames as f64;
    let memcpy_gbps = (frame_bytes * frames) as f64 / duration_memcpy.as_secs_f64() / 1e9;
    let cpu_load_60fps = (memcpy_per_frame / (1000.0 / 60.0)) * 100.0;
    println!("    -> Total CPU copy time  : {:.2?}", duration_memcpy);
    println!("    -> Per-frame copy time  : {:.3} ms", memcpy_per_frame);
    println!("    -> Memory Bus Bandwidth : {:.2} GB/s", memcpy_gbps);
    println!("    -> CPU Core Saturation  : {:.1}% of 1 core consumed solely by memcpy at 60 FPS", cpu_load_60fps);
    println!();

    // Benchmark 2: in-process NVENC reading a registered host buffer (device copy when registration fails)
    #[cfg(feature = "inproc-av")]
    {
        extern "C" {
            fn nvenc_create(config: *const NvencConfigFFI) -> *mut std::ffi::c_void;
            fn nvenc_register_host_buffer(
                engine: *mut std::ffi::c_void,
                host_ptr: *mut std::ffi::c_void,
                size: usize,
            ) -> i32;
            fn nvenc_encode_frame(
                engine: *mut std::ffi::c_void,
                input_argb: *const u8,
                pitch: u32,
                force_idr: i32,
                out_data: *mut *const u8,
                out_len: *mut u32,
            ) -> i32;
            fn nvenc_destroy(engine: *mut std::ffi::c_void);
        }

        // Must match NvencConfig in c_engine/nvenc_engine.h.
        #[repr(C)]
        struct NvencConfigFFI {
            width: u32,
            height: u32,
            fps: u32,
            gop: u32,
            cq: u32,
            target_bps: i64,
            max_bps: i64,
            buffer_bits: i64,
            is_hevc: i32,
            chroma_444: i32,
        }

        let cfg = NvencConfigFFI {
            width,
            height,
            fps: 60,
            gop: 120,
            cq: 18,
            target_bps: 16_000_000,
            max_bps: 30_000_000,
            buffer_bits: 500_000,
            is_hevc: 1, // H.265 / HEVC
            chroma_444: i32::from(std::env::var("UU_BENCH_CHROMA").as_deref() == Ok("444")),
        };

        println!("[2] In-process NVENC H.265 encoding");
        unsafe {
            let engine = nvenc_create(&cfg);
            if engine.is_null() {
                println!("    [SKIP] NVENC driver not available on this system.");
            } else {
                let registered = nvenc_register_host_buffer(engine, host_pixels.as_mut_ptr() as *mut _, frame_bytes) == 0;
                println!("    -> Input path           : {}", if registered { "registered host buffer (DMA)" } else { "cuMemcpyHtoD copy" });
                let input = if registered { std::ptr::null() } else { host_pixels.as_ptr() };
                let mut out_data: *const u8 = std::ptr::null();
                let mut out_len: u32 = 0;
                // Warmup
                let _ = nvenc_encode_frame(engine, input, width * 4, 1, &mut out_data, &mut out_len);

                let start = Instant::now();
                let (mut encoded_bytes, mut encoded_frames) = (0u64, 0usize);
                for i in 0..frames {
                    if nvenc_encode_frame(engine, input, width * 4, i32::from(i == 0), &mut out_data, &mut out_len) == 0 {
                        encoded_bytes += u64::from(out_len);
                        encoded_frames += 1;
                    }
                }
                let elapsed = start.elapsed();
                if encoded_frames == 0 {
                    println!("    [FAIL] no frame was encoded");
                } else {
                    let per_frame = elapsed.as_secs_f64() * 1000.0 / encoded_frames as f64;
                    println!("    -> Encoded frames       : {encoded_frames}/{frames} in {elapsed:.2?}");
                    println!("    -> Per-frame latency    : {per_frame:.3} ms ({:.1} FPS max)", 1000.0 / per_frame);
                    println!("    -> Output bitrate       : {:.2} Mbps average", encoded_bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1e6);
                    if registered {
                        println!("    -> Host-to-device copy  : none ({:.0} MB/s at 60 fps avoided)", frame_bytes as f64 * 60.0 / 1024.0 / 1024.0);
                    }
                }
                nvenc_destroy(engine);
            }
        }
    }
}
