fn main() {
    println!("cargo:rerun-if-changed=c_engine/portal_clipboard.inc");
    println!("cargo:rerun-if-changed=c_engine/x11_engine.c");
    println!("cargo:rerun-if-changed=c_engine/clipboard.c");
    println!("cargo:rerun-if-changed=c_engine/assist_window.c");
    println!("cargo:rerun-if-changed=c_engine/pam_check.c");
    println!("cargo:rerun-if-env-changed=UU_NO_GTK");
    println!("cargo:rustc-check-cfg=cfg(uu_gtk)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        if std::env::var_os("CARGO_FEATURE_KMS").is_some() {
            println!("cargo:rerun-if-changed=c_engine/kms_engine.c");
            let output=std::process::Command::new("pkg-config").args(["--cflags","libdrm"]).output().expect("KMS requires pkg-config and libdrm development headers");
            assert!(output.status.success(), "KMS requires libdrm development headers");
            let mut build=cc::Build::new();
            build.file("c_engine/kms_engine.c");
            for flag in String::from_utf8_lossy(&output.stdout).split_whitespace() { build.flag(flag); }
            build.compile("uu_kms_engine");
            println!("cargo:rustc-link-lib=drm");
        }
        cc::Build::new()
            .file("c_engine/x11_engine.c")
            .file("c_engine/clipboard.c")
            .file("c_engine/pam_check.c")
            .compile("x11_engine");

        println!("cargo:rustc-link-lib=X11");
        println!("cargo:rustc-link-lib=Xext");
        println!("cargo:rustc-link-lib=Xtst");
        println!("cargo:rustc-link-lib=Xfixes");
        println!("cargo:rustc-link-lib=dl"); // pam_check.c loads libpam at runtime
        // Optional GTK3 login window: built when gtk+-3.0 is installed, else `login` stays text-only.
        let gtk = |arg: &str| std::process::Command::new("pkg-config").args([arg, "gtk+-3.0"]).output().ok()
            .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        match (std::env::var_os("UU_NO_GTK").is_none()).then(|| gtk("--cflags").zip(gtk("--libs"))).flatten() {
            Some((cflags, libs)) => {
                let mut build = cc::Build::new();
                build.file("c_engine/assist_window.c").flag_if_supported("-Wno-deprecated-declarations");
                for flag in cflags.split_whitespace() { build.flag(flag); }
                build.compile("uu_login_window");
                for flag in libs.split_whitespace() {
                    if let Some(lib) = flag.strip_prefix("-l") { println!("cargo:rustc-link-lib={lib}"); }
                    else if let Some(dir) = flag.strip_prefix("-L") { println!("cargo:rustc-link-search=native={dir}"); }
                }
                println!("cargo:rustc-link-lib=pthread");
                println!("cargo:rustc-cfg=uu_gtk");
            }
            None => println!("cargo:warning=gtk+-3.0 not found: building without the login window"),
        }
        if std::env::var_os("CARGO_FEATURE_PORTAL").is_some() {
            println!("cargo:rerun-if-changed=c_engine/portal_engine.c");
            println!("cargo:rerun-if-changed=c_engine/portal_engine.h");
            println!("cargo:rerun-if-changed=c_engine/portal_eis.inc");
            println!("cargo:rerun-if-changed=c_engine/desktop_power.c");
            let flags=|kind:&str| {
                let output=std::process::Command::new("pkg-config").args([kind,"gio-2.0","gio-unix-2.0","libpipewire-0.3","libei-1.0 >= 1.1"]).output()
                    .expect("Install pkg-config and GIO/PipeWire/libei development libraries");
                assert!(output.status.success(),"Portal requires GIO, PipeWire and libei >= 1.1 development libraries");
                String::from_utf8(output.stdout).expect("pkg-config flags must be UTF-8")
            };
            let mut build=cc::Build::new();
            build.file("c_engine/portal_engine.c").file("c_engine/desktop_power.c").flag_if_supported("-Wno-unused-parameter");
            for flag in flags("--cflags").split_whitespace() { build.flag(flag); }
            build.compile("uu_portal_engine");
            for flag in flags("--libs").split_whitespace() {
                if let Some(lib)=flag.strip_prefix("-l") { println!("cargo:rustc-link-lib={lib}"); }
                else if let Some(dir)=flag.strip_prefix("-L") { println!("cargo:rustc-link-search=native={dir}"); }
            }
            println!("cargo:rustc-link-lib=pthread");
        }
        if std::env::var_os("CARGO_FEATURE_WAYLAND").is_some() {
            let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
            let mut build = cc::Build::new();
            build.flag_if_supported("-Wno-unused-parameter"); // protocol listener signatures
            build.file("c_engine/wayland_engine.c").include(&out);
            for name in ["wlr-screencopy-unstable-v1", "wlr-virtual-pointer-unstable-v1", "virtual-keyboard-unstable-v1", "wlr-output-power-management-unstable-v1", "linux-dmabuf-unstable-v1"] {
                let xml = format!("c_engine/wayland-protocols/{name}.xml");
                println!("cargo:rerun-if-changed={xml}");
                for (mode, suffix) in [("client-header", "client-protocol.h"), ("private-code", "protocol.c")] {
                    let target = out.join(format!("{name}-{suffix}"));
                    let status = std::process::Command::new("wayland-scanner").args([mode, &xml]).arg(&target).status()
                        .expect("Install wayland-scanner to build the Wayland backend");
                    assert!(status.success(), "wayland-scanner failed");
                    if mode == "private-code" { build.file(target); }
                }
            }
            println!("cargo:rerun-if-changed=c_engine/wayland_engine.c");
            build.compile("uu_wayland_engine");
            println!("cargo:rustc-link-lib=wayland-client");
            println!("cargo:rustc-link-lib=xkbcommon");
        }
    }
}
