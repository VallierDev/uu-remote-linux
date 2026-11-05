#!/usr/bin/env python3
"""Private PipeWire native-fence and CUDA/NVENC synthetic test; no capture.

Requires matching NVIDIA DRM/CUDA device, EGL native fences, NVENC, PipeWire and ffmpeg.
No Portal, revocation or proof that explicit fences were necessary is claimed.
"""
import argparse
import os
import pathlib
import signal
import shlex
import sys
import subprocess
import tempfile
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from pipewire_synthetic_smoke import CONFIG, command, spawn, terminate


def main(device=None):
    signals = {signal.SIGINT, signal.SIGTERM}
    original_mask = signal.pthread_sigmask(signal.SIG_BLOCK, set())
    original_handlers = {sig: signal.getsignal(sig) for sig in signals}
    signal.signal(signal.SIGINT, terminate)
    signal.signal(signal.SIGTERM, terminate)
    children = []
    with tempfile.TemporaryDirectory(prefix='uu-pw-video-') as directory:
        root = pathlib.Path(directory)
        config = root / 'private.conf'
        config.write_text(CONFIG)
        binary = root / 'probe'
        source = pathlib.Path(__file__).with_suffix('.c')
        # Each child is registered before SIGINT or SIGTERM can unwind, including builds.
        try:
            pkg = spawn(children, ['pkg-config', '--cflags', '--libs', 'libpipewire-0.3', 'gbm', 'libdrm', 'egl', 'glesv2'],
                        text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            stdout, stderr = pkg.communicate(timeout=10)
            if pkg.returncode:
                raise RuntimeError('required build dependencies unavailable (libpipewire-0.3, gbm, libdrm): ' + stderr.strip())
            flags = shlex.split(stdout)
            # CFLAGS/LDFLAGS allow e.g. -fsanitize=address,undefined at compile and link.
            compiler = spawn(children, [*shlex.split(os.environ.get('CC', 'cc')),
                             '-std=gnu11', '-Wall', '-Wextra', '-Werror', '-O1', '-g',
                             '-I' + str(source.parents[2] / 'vendor/ffnvcodec'),
                             *shlex.split(os.environ.get('CFLAGS', '')), str(source),
                             str(source.parents[2] / 'c_engine/portal_nvenc_spa.c'),
                             str(source.parents[2] / 'c_engine/portal_nvenc_timeline.c'),
                             str(source.parents[2] / 'c_engine/portal_nvenc_egl_attrs.c'),
                             str(source.parents[2] / 'contrib/portal-nvenc-experimental.c'),
                             '-o', str(binary), *flags, '-ldl', *shlex.split(os.environ.get('LDFLAGS', ''))])
            if compiler.wait(timeout=30):
                raise subprocess.CalledProcessError(compiler.returncode, compiler.args)
            env = {k: v for k, v in os.environ.items()
                   if not k.startswith(('PIPEWIRE_', 'SPA_', 'PW_'))
                   and k not in ('XDG_RUNTIME_DIR', 'XDG_CONFIG_HOME', 'XDG_CONFIG_DIRS')}
            env.update(PIPEWIRE_RUNTIME_DIR=directory, XDG_RUNTIME_DIR=directory,
                       XDG_CONFIG_HOME=directory, XDG_CONFIG_DIRS=directory,
                       PIPEWIRE_REMOTE=str(root / 'pipewire-0'))
            daemon = spawn(children, ['pipewire', '-c', str(config)], env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            deadline = time.monotonic() + 5
            while not (root / 'pipewire-0').is_socket():
                if daemon.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('private daemon failed to start')
                time.sleep(.05)
            bitstream = root / 'synthetic.h264'
            probe = spawn(children, [str(binary), device or '/dev/dri/renderD128', str(bitstream)], env=env)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if probe.poll() is not None:
                    if probe.returncode == 77:
                        return 77
                    raise RuntimeError(f'probe exited before link: {probe.returncode}')
                if daemon.poll() is not None:
                    raise RuntimeError('private daemon exited before link')
                # Raw video streams have a single port each; require exact node prefix
                # and a unique numeric ID rather than connecting arbitrary ports.
                ports = []
                for direction, prefix in [('-o', 'uu-dmabuf-producer:'), ('-i', 'uu-dmabuf-consumer:')]:
                    lines = command(children, env, 'pw-link', '-I', direction).splitlines()
                    matches = [line.strip().split(None, 1) for line in lines if len(line.strip().split(None, 1)) == 2 and line.strip().split(None, 1)[1].startswith(prefix)]
                    if len(matches) > 1:
                        raise RuntimeError('ambiguous video port')
                    ports.append(matches[0][0] if matches else None)
                if all(ports) and ports[0] != ports[1]:
                    command(children, env, 'pw-link', *ports)
                    break
                time.sleep(.05)
            else:
                raise TimeoutError('video ports not found')
            result = probe.wait(timeout=15)
            if result == 77:
                return 77
            if result:
                raise RuntimeError(f'DMA-BUF probe failed: {result}')
            dimensions = spawn(children, ['ffprobe', '-v', 'error', '-select_streams', 'v:0',
                               '-show_entries', 'stream=width,height', '-of', 'csv=s=x:p=0', str(bitstream)],
                               env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            out, err = dimensions.communicate(timeout=20)
            if dimensions.returncode or out != b'256x256\n':
                raise RuntimeError('ffprobe dimension verification failed: ' + err.decode(errors='replace'))
            decoder = spawn(children, ['ffmpeg', '-nostdin', '-v', 'error', '-c:v', 'h264', '-f', 'h264',
                            '-i', str(bitstream), '-frames:v', '1', '-f', 'rawvideo', '-pix_fmt', 'rgb24', 'pipe:1'],
                            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            pixels, err = decoder.communicate(timeout=20)
            if decoder.returncode or len(pixels) != 256 * 256 * 3:
                raise RuntimeError('ffmpeg software decode failed or wrong size: ' + err.decode(errors='replace'))
            expected = ((255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255))
            for q, color in enumerate(expected):
                for dy in range(16, 112, 8):
                    for dx in range(16, 112, 8):
                        x, y = (q % 2) * 128 + dx, (q // 2) * 128 + dy
                        pixel = pixels[(y * 256 + x) * 3:(y * 256 + x) * 3 + 3]
                        if any(abs(a - b) > 35 for a, b in zip(pixel, color)):
                            raise RuntimeError(f'decoded synthetic color mismatch quadrant={q} x={x} y={y}')
            print('Software decoded 256x256 synthetic IDR: 144 samples per quadrant within tolerance 35')
        finally:
            signal.pthread_sigmask(signal.SIG_BLOCK, signals)
            try:
                for sig in signals:
                    signal.signal(sig, signal.SIG_IGN)
                errors = []
                # Only direct children are owned; descendants lack a safe group identity.
                for child in reversed(children):
                    try:
                        if child.poll() is None:
                            child.terminate()
                    except Exception as exc:
                        errors.append(exc)
                for child in reversed(children):
                    try:
                        try:
                            child.wait(timeout=2)
                        except subprocess.TimeoutExpired:
                            child.kill()
                            child.wait(timeout=2)
                    except Exception as exc:
                        errors.append(exc)
                if errors:
                    print('child cleanup errors: ' + repr(errors), file=sys.stderr)
                    raise RuntimeError('failed to clean up all child processes')
            finally:
                # Discard signals received during cleanup before restoring dispositions.
                while signal.sigtimedwait(signals, 0) is not None:
                    pass
                for sig, handler in original_handlers.items():
                    signal.signal(sig, handler)
                signal.pthread_sigmask(signal.SIG_SETMASK, original_mask)
    print('PASS: private synthetic native-fence/CUDA/NVENC and software decode; no Portal, revocation or fence-necessity claim')
    return 0


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', help='trusted local /dev/dri/renderD<digits> (default: /dev/dri/renderD128)')
    args = parser.parse_args()
    try:
        sys.exit(main(args.device))
    except FileNotFoundError as exc:
        print(f'FAIL: required executable not found: {exc.filename}', file=sys.stderr)
        sys.exit(1)
    except (RuntimeError, subprocess.CalledProcessError, subprocess.TimeoutExpired, TimeoutError) as exc:
        print(f'FAIL: {exc}', file=sys.stderr)
        sys.exit(1)
