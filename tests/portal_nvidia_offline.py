#!/usr/bin/env python3
"""Build/run offline Portal/NVIDIA regressions; no GPU access or desktop sharing."""
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def flags(*packages):
    return shlex.split(subprocess.check_output(
        ['pkg-config', '--cflags', '--libs', *packages], text=True))


def main():
    os.chdir(ROOT)
    if not shutil.which("dbus-daemon"):
        raise SystemExit("dbus-daemon is required for private-bus regression tests")
    base = ['cc', '-std=gnu11', '-O1', '-g', '-Wall', '-Wextra',
            '-Wno-unused-parameter', '-fsanitize=address,undefined',
            '-fno-omit-frame-pointer', '-no-pie']
    env = dict(os.environ, ASAN_OPTIONS='detect_leaks=1', UBSAN_OPTIONS='halt_on_error=1')
    modern = subprocess.run(['pkg-config', '--atleast-version=1.2', 'libpipewire-0.3']).returncode == 0
    with tempfile.TemporaryDirectory(prefix='portal-offline-') as tmp:
        def run(name, sources, libs):
            binary = str(Path(tmp) / name)
            subprocess.run(base + sources + ['-o', binary] + libs, check=True)
            subprocess.run([binary], check=True, env=env, timeout=45)
            print('PASS', name, flush=True)

        drm = flags('libdrm', 'libspa-0.2')
        run('timeline_legacy', ['tests/portal_nvenc_timeline_legacy.c'], drm)
        nv = ['-Ivendor/ffnvcodec', '-ldl', '-lpthread']
        for name in ['nvenc_gpu', 'nvenc_egl', 'nvenc_egl_inspect', 'nvenc_device']:
            run(name, [f'tests/{name}.c'], nv)
        egl = flags('egl', 'libdrm', 'libspa-0.2')
        run('egl_attributes', ['contrib/test-portal-nvenc-egl-attrs.c',
                              'c_engine/portal_nvenc_egl_attrs.c'], egl)
        run('egl_image', ['tests/portal_nvenc_image.c', 'c_engine/portal_nvenc_image.c',
                          'c_engine/portal_nvenc_egl_attrs.c'], egl)
        portal = flags('gio-2.0', 'gio-unix-2.0', 'libpipewire-0.3', 'libei-1.0') + ['-lpthread', '-lm']
        for name in ['portal_engine', 'portal_dmabuf', 'portal_session_close', 'portal_cancel_dbus']:
            run(name, [f'tests/{name}.c'], portal)
        if modern:
            for name in ['portal_async_lease', 'portal_explicit_transport', 'portal_nvenc_lease_lifecycle']:
                run(name, [f'tests/{name}.c'], portal)
            run('timeline', ['tests/portal_nvenc_timeline.c', 'c_engine/portal_nvenc_timeline.c'], drm)
            run('spa_snapshot', ['tests/portal_nvenc_spa.c', 'c_engine/portal_nvenc_spa.c'], flags('libspa-0.2'))
        else:
            print('SKIP explicit-sync metadata tests: PipeWire headers < 1.2', flush=True)


if __name__ == '__main__':
    main()
