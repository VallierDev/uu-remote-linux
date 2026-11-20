{
  self,
  inprocAvPackage,
}:

{ pkgs, ... }:

let
  dummyCudaLib = pkgs.runCommandCC "dummy-cuda" { } ''
    mkdir -p $out/lib
    $CC -shared -x c /dev/null -o $out/lib/libcuda.so.1
  '';

  swayConfig = pkgs.writeText "sway-headless.conf" ''
    xwayland disable
    exec sh -c 'systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE && systemctl --user start test-wayland-session.target'
  '';
in
{
  name = "uu-remote-linux-nixos";

  node.pkgsReadOnly = false;

  nodes.machine =
    { config, ... }:
    {
      imports = [ self.nixosModules.default ];

      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };

      services.getty.autologinUser = "alice";

      services.uu-remote-linux = {
        enable = true;
        uinput.enable = true;
        settings = {
          UU_DESKTOP_BACKEND = "wayland";
          UU_LOGIN_WINDOW = "0";
          UU_REMOTE_ASSIST = "0";
        };
      };

      # graphical-session.target has RefuseManualStart=yes; pull it in via a session target
      # after Sway exports WAYLAND_DISPLAY (mirroring a real display manager / session wrapper).
      systemd.user.targets.test-wayland-session.unitConfig = {
        Wants = [ "graphical-session.target" ];
        Before = [ "graphical-session.target" ];
      };

      # Intentionally omit ffmpeg and wl-clipboard from systemPackages so the test
      # verifies the wrapped binaries provide their own runtime tools.
      environment.systemPackages = [
        pkgs.sway-unwrapped
        pkgs.patchelf
        pkgs.xvfb-run
      ];
    };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@1000.service")

    def as_alice(cmd):
        return f"su - alice -c 'export XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; {cmd}'"

    with subtest("uinput udev rule grants active seat user access"):
        machine.wait_until_succeeds(as_alice("test -w /dev/uinput"))

    with subtest("wrapped runtime tools stay isolated from global PATH"):
        machine.fail(as_alice("command -v ffmpeg"))
        machine.fail(as_alice("command -v wl-copy"))
        machine.fail(as_alice("command -v wl-paste"))

    with subtest("headless Wayland session starts uuremote-native user service"):
        machine.succeed(
            as_alice(
                "systemd-run --user --unit=test-sway "
                "-p Environment=WLR_BACKENDS=headless "
                "-p Environment=WLR_RENDERER=pixman "
                "-p Environment=WLR_LIBINPUT_NO_DEVICES=1 "
                "-p Environment=XDG_CURRENT_DESKTOP=sway "
                "-p Environment=XDG_SESSION_TYPE=wayland "
                "sway -c ${swayConfig}"
            )
        )
        machine.wait_until_succeeds(as_alice("systemctl --user is-active uuremote-native.service"))
        machine.wait_until_succeeds(as_alice("test -s ~/.config/uuremote/host.pid"))

    with subtest("host process uses libexec ELF, module settings, and wl-clipboard on PATH"):
        pid = machine.succeed(as_alice("cat ~/.config/uuremote/host.pid")).strip()
        exe_path = machine.succeed(f"readlink -f /proc/{pid}/exe").strip()
        assert exe_path.endswith("/libexec/uu-remote-linux/uuremote-server"), f"unexpected exe: {exe_path}"

        raw_env = machine.succeed(f"tr '\\0' '\\n' < /proc/{pid}/environ")
        env_map = dict(line.split("=", 1) for line in raw_env.splitlines() if "=" in line)
        assert env_map.get("UU_DESKTOP_BACKEND") == "wayland", env_map
        assert env_map.get("UU_LOGIN_WINDOW") == "0", env_map
        assert env_map.get("UU_REMOTE_ASSIST") == "0", env_map

        wrapped_path = env_map["PATH"]
        assert "ffmpeg" in wrapped_path, f"ffmpeg missing from wrapped PATH: {wrapped_path}"
        assert "wl-clipboard" in wrapped_path, f"wl-clipboard missing from wrapped PATH: {wrapped_path}"

        wayland_display = env_map["WAYLAND_DISPLAY"]
        machine.succeed(
            as_alice(
                f"PATH={wrapped_path} WAYLAND_DISPLAY={wayland_display} wl-copy 'uu-nix-clipboard-ok'"
            )
        )
        pasted = machine.succeed(
            as_alice(
                f"PATH={wrapped_path} WAYLAND_DISPLAY={wayland_display} wl-paste --no-newline"
            )
        )
        assert pasted == "uu-nix-clipboard-ok", f"unexpected clipboard content: {pasted!r}"

    with subtest("doctor, SIGUSR2 reload, and Wayland capture/encode probes succeed"):
        doctor = machine.succeed(
            as_alice(
                f"WAYLAND_DISPLAY={wayland_display} XDG_CURRENT_DESKTOP=sway XDG_SESSION_TYPE=wayland uuremote-server doctor"
            )
        )
        assert "selected backend       Wayland" in doctor, doctor
        assert "/dev/uinput            writable" in doctor, doctor
        assert "libx264" in doctor, doctor
        assert "service                active" in doctor, doctor

        set_enc = machine.succeed(as_alice("uuremote-server set-encoder cpu"))
        assert "notified the running host" in set_enc, set_enc
        machine.succeed(as_alice("uuremote-server set-encoder auto"))

        probe = machine.succeed(
            as_alice(
                f"WAYLAND_DISPLAY={wayland_display} XDG_CURRENT_DESKTOP=sway uuremote-server desktop-probe"
            )
        )
        assert "backend=Wayland" in probe and "frame_bytes=0" not in probe, probe

        video_probe = machine.succeed(
            as_alice(
                f"WAYLAND_DISPLAY={wayland_display} XDG_CURRENT_DESKTOP=sway UU_VIDEO_ENCODER=cpu UU_PROBE_FRAMES=2 uuremote-server desktop-video-probe"
            )
        )
        assert "Encoded capture probe: frames=2" in video_probe, video_probe

    with subtest("packaged systemd user unit and viewer wrapper"):
        unit_text = machine.succeed("cat /run/current-system/sw/lib/systemd/user/uuremote-native.service")
        assert "ExecStart=/nix/store/" in unit_text and "/bin/uuremote-server" in unit_text, unit_text

        machine.succeed(as_alice("uuremote-viewer --help >/dev/null"))
        viewer_bin = machine.succeed("command -v uuremote-viewer").strip()
        machine.succeed(f"grep -a -q wl-clipboard {viewer_bin} && grep -a -q ffmpeg {viewer_bin}")

    with subtest("inproc-av driver RUNPATH resolves /run/opengl-driver/lib"):
        inproc_exe = "${inprocAvPackage}/libexec/uu-remote-linux/uuremote-server"
        rpath = machine.succeed(f"patchelf --print-rpath {inproc_exe}").strip()
        assert rpath.startswith("/run/opengl-driver/lib:"), f"missing driver runpath: {rpath}"

        machine.succeed(
            "mkdir -p /run/opengl-driver/lib && "
            "ln -sfn ${dummyCudaLib}/lib/libcuda.so.1 /run/opengl-driver/lib/libcuda.so.1"
        )
        nvenc_probe = machine.succeed(
            as_alice(
                'xvfb-run -a -s "-screen 0 1280x720x24" '
                "env -u WAYLAND_DISPLAY XDG_SESSION_TYPE=x11 UU_DESKTOP_BACKEND=x11 "
                "UU_VIDEO_ENCODER=nvenc UU_PROBE_FRAMES=2 "
                "${inprocAvPackage}/bin/uuremote-server desktop-video-probe 2>&1"
            )
        )
        assert "dlopen libcuda.so.1 failed" not in nvenc_probe, nvenc_probe
        assert "Missing CUDA driver symbols" in nvenc_probe, nvenc_probe
        assert "Encoded capture probe: frames=2" in nvenc_probe, nvenc_probe
  '';
}
