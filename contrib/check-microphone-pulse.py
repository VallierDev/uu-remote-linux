#!/usr/bin/env python3
"""Run the opt-in microphone sink-loss regression on a private PulseAudio daemon.

No hardware modules or default configuration are loaded. The Rust test generates
audio in memory and observes only its own null sinks. Requires pactl and parec.
在独立 PulseAudio 服务中运行麦克风回归；仅使用空设备及内存生成音，需要 pactl、parec。
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", default=shutil.which("pulseaudio"), help="PulseAudio 可执行文件 / PulseAudio executable")
    parser.add_argument("--module-dir", help="解压包的模块目录 / module directory for an unpacked package")
    parser.add_argument("--test-binary", help="已有 Cargo 测试程序；默认构建 / existing Cargo test executable; otherwise build")
    parser.add_argument("--smoke", action="store_true", help="验证生成音传输 / check generated audio delivery instead of target loss")
    args = parser.parse_args()
    if not args.daemon:
        parser.error("请用 --daemon 指定 PulseAudio，无需系统服务 / provide --daemon; no system service is needed")
    repo = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix="uu-microphone-test-") as temp:
        private = Path(temp)
        for name in ["runtime", "state", "config", "cache"]:
            (private / name).mkdir(mode=0o700)
        cookie = private / "cookie"
        cookie.write_bytes(os.urandom(256))
        socket = private / "runtime/native"
        env = os.environ.copy()
        env.update(
            XDG_RUNTIME_DIR=str(private / "runtime"), XDG_CONFIG_HOME=str(private / "config"),
            XDG_STATE_HOME=str(private / "state"), XDG_CACHE_HOME=str(private / "cache"),
            PULSE_RUNTIME_PATH=str(private / "runtime"), PULSE_STATE_PATH=str(private / "state"),
            PULSE_COOKIE=str(cookie), PULSE_SERVER="unix:" + str(socket), UU_MICROPHONE_ISOLATED_TEST="1",
            UU_MICROPHONE="1",
        )
        command = [args.daemon, "-n", "--daemonize=no", "--use-pid-file=no", "--exit-idle-time=-1",
                   "--disable-shm=yes", "--realtime=no", "--high-priority=no",
                   "--load=module-native-protocol-unix socket=" + str(socket)
                   + " auth-anonymous=1 auth-cookie=" + str(cookie)]
        if args.module_dir:
            command.append("--dl-search-path=" + args.module_dir)
        with (private / "daemon.log").open("w") as log:
            daemon = subprocess.Popen(command, env=env, stdout=log, stderr=log)
            try:
                for _ in range(100):
                    if daemon.poll() is not None:
                        raise RuntimeError("独立服务启动失败 / isolated server failed:\n" + (private / "daemon.log").read_text()[-2000:])
                    if socket.exists() and subprocess.run(
                        ["pactl", "--server=" + env["PULSE_SERVER"], "info"], env=env,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    ).returncode == 0:
                        break
                    time.sleep(0.03)
                else:
                    raise RuntimeError("独立 PulseAudio 未就绪 / isolated PulseAudio did not become ready")
                test = "microphone::tests::pulse_target_loss_never_routes_to_default_output"
                if args.smoke:
                    test = "microphone::tests::pipewire_virtual_microphone_receives_generated_tone"
                if args.test_binary:
                    command = [str(Path(args.test_binary).resolve()), test, "--ignored", "--exact"]
                else:
                    command = ["cargo", "test", "--locked", "--bin", "uuremote-server", test,
                               "--", "--ignored", "--exact"]
                listing = subprocess.run(command + ["--list", "--color=never"], cwd=repo, env=env,
                                         capture_output=True, text=True)
                if listing.returncode:
                    print(listing.stderr, file=sys.stderr)
                    return listing.returncode
                if f"{test}: test" not in listing.stdout.splitlines():
                    parser.error("目标测试不存在，拒绝零项成功 / requested test is absent; refusing a zero-test success")
                return subprocess.run(command, cwd=repo, env=env).returncode
            finally:
                if daemon.poll() is None:
                    daemon.terminate()
                    try:
                        daemon.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        daemon.kill()
                        daemon.wait()


if __name__ == "__main__":
    raise SystemExit(main())
