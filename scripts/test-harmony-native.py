#!/usr/bin/env python3
"""Run the production pointer adapter against fake OH_Input calls on macOS."""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
deveco = Path(os.environ.get('DEVECO_HOME', '/Applications/DevEco-Studio.app'))
headers = deveco / 'Contents/sdk/default/openharmony/native/sysroot/usr/include'
with tempfile.TemporaryDirectory(prefix='uu-harmony-input-') as output:
    binary = Path(output) / 'pointer_events_test'
    # Unused media/UI entry points are removed by the linker. System headers
    # precede the OHOS SDK so its libc does not shadow the host's libc.
    subprocess.run([
        'clang++', '-std=c++17', '-fno-exceptions', '-Wno-ignored-attributes',
        '-idirafter', str(headers), '-Wl,-dead_strip',
        str(ROOT / 'src/ohos/engine.cc'),
        str(ROOT / 'tests/harmony/pointer_events_test.cc'), '-o', str(binary),
    ], check=True)
    subprocess.run([str(binary)], check=True)
