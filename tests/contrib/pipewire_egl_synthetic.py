#!/usr/bin/env python3
"""Private PipeWire synthetic GBM-to-EGL offscreen pixel test; no desktop capture.

Requires a trusted render node supporting GBM modifier 0x0300000000606010,
EGL/GBM and PipeWire. This is not a Portal, CUDA or NVENC test; producer
completion is explicitly established by glFinish.
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
                             *shlex.split(os.environ.get('CFLAGS', '')), str(source),
                             str(source.parents[2] / 'c_engine/portal_nvenc_egl_attrs.c'),
                             '-o', str(binary), *flags, *shlex.split(os.environ.get('LDFLAGS', ''))])
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
            probe = spawn(children, [str(binary)] + ([device] if device is not None else []), env=env)
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
    print('PASS: synthetic offscreen PipeWire/EGL test; direct children cleaned up')
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
