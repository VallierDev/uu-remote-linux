#!/usr/bin/env python3
"""Build the HarmonyOS host with the installed DevEco SDK, without Linux libraries."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def run(args, env, cwd=ROOT):
    subprocess.run([str(arg) for arg in args], env=env, cwd=cwd, check=True)


def prepare_rust_project(destination, lockfile):
    """Keep OHOS targets and dependency overrides out of the Linux package."""
    manifest = (ROOT / 'Cargo.toml').read_text()
    if '[lib]' in manifest or manifest.count('[patch.crates-io]\n') != 1:
        raise RuntimeError('Unexpected root manifest; review the OHOS manifest overlay')
    manifest = manifest.replace('[patch.crates-io]\n',
                                '[patch.crates-io]\nwebrtc-util = { path = "vendor/webrtc-util" }\n', 1)
    manifest += ('\n[lib]\nname = "uu_remote_harmony"\npath = "src/harmony_lib.rs"\n'
                 'crate-type = ["staticlib"]\ntest = false\nbench = false\n\n[workspace]\n')
    destination.mkdir(parents=True, exist_ok=True)
    for name in ['src', 'vendor', 'c_engine', 'benches', 'build.rs', 'README.md']:
        source = ROOT / name
        link = destination / name
        if link.is_symlink() and link.resolve() == source.resolve():
            continue
        if link.exists() or link.is_symlink():
            raise RuntimeError(f'Unexpected file in generated Rust project: {link}')
        link.symlink_to(source, target_is_directory=source.is_dir())
    (destination / 'Cargo.toml').write_text(manifest)
    shutil.copy2(lockfile, destination / 'Cargo.lock')
    return destination / 'Cargo.toml'


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--skip-rust', action='store_true')
    parser.add_argument('--native-only', action='store_true')
    parser.add_argument('--update-lock', action='store_true',
                        help='Refresh the OHOS lockfile from the Linux lockfile before building')
    args = parser.parse_args()
    deveco = Path(os.environ.get('DEVECO_HOME', '/Applications/DevEco-Studio.app'))
    native = deveco / 'Contents/sdk/default/openharmony/native'
    tools = deveco / 'Contents/tools'
    output = Path(os.environ.get('UU_HARMONY_BUILD', str(Path.home() / 'Library/Caches/uu-remote-harmony')))
    output.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(output / 'target')
    env['CC_aarch64_unknown_linux_ohos'] = str(native / 'llvm/bin/clang')
    env['CXX_aarch64_unknown_linux_ohos'] = str(native / 'llvm/bin/clang++')
    env['AR_aarch64_unknown_linux_ohos'] = str(native / 'llvm/bin/llvm-ar')
    flags = f'--target=aarch64-linux-ohos --sysroot={native}/sysroot'
    env['CFLAGS_aarch64_unknown_linux_ohos'] = flags
    env['CXXFLAGS_aarch64_unknown_linux_ohos'] = flags
    rust_project = output / 'rust'
    lockfile = ROOT / ('Cargo.lock' if args.update_lock else 'harmony/Cargo.lock')
    manifest = prepare_rust_project(rust_project, lockfile)
    if args.update_lock:
        run(['cargo', 'update', '--manifest-path', manifest, '-p', 'webrtc-util'], env, rust_project)
        shutil.copy2(rust_project / 'Cargo.lock', ROOT / 'harmony/Cargo.lock')
    if not args.skip_rust:
        run(['cargo', 'build', '--manifest-path', manifest, '--locked', '--lib', '--release',
             '--target', 'aarch64-unknown-linux-ohos'], env, rust_project)
    library = output / 'target/aarch64-unknown-linux-ohos/release/libuu_remote_harmony.a'
    run(['cmake', '-S', ROOT / 'harmony/entry/src/main/cpp', '-B', output / 'native', '-G', 'Ninja',
         f'-DCMAKE_TOOLCHAIN_FILE={native}/build/cmake/ohos.toolchain.cmake', '-DOHOS_ARCH=arm64-v8a',
         '-DOHOS_PLATFORM=OHOS', '-DCMAKE_BUILD_TYPE=Release', f'-DUU_RUST_LIBRARY={library}'], env)
    run(['cmake', '--build', output / 'native'], env)
    if args.native_only:
        return
    project = output / 'project'
    shutil.copytree(ROOT / 'harmony', project, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns('oh_modules', '.hvigor', 'build', '.idea',
                                                  'libs', 'local.properties', 'signing.local.json'))
    config = json.loads((project / 'build-profile.json5').read_text())
    signing = ROOT / 'harmony/signing.local.json'
    if signing.exists():
        config['app'].update(json.loads(signing.read_text())['app'])
    for product in config['app']['products']:
        if any(item['name'] == product['name'] for item in config['app'].get('signingConfigs', [])):
            product['signingConfig'] = product['name']
    (project / 'build-profile.json5').write_text(json.dumps(config, indent=2) + '\n')
    libs = project / 'entry/libs/arm64-v8a'
    libs.mkdir(parents=True, exist_ok=True)
    shutil.copy2(output / 'native/libentry.so', libs / 'libentry.so')
    shutil.copy2(native / 'llvm/lib/aarch64-linux-ohos/libc++_shared.so', libs / 'libc++_shared.so')
    env['DEVECO_SDK_HOME'] = str(deveco / 'Contents/sdk')
    env['JAVA_HOME'] = str(deveco / 'Contents/jbr/Contents/Home')
    env['PATH'] = str(tools / 'node/bin') + os.pathsep + env['PATH']
    node = tools / 'node/bin/node'
    run([node, tools / 'ohpm/bin/pm-cli.js', 'install', '--all'], env, project)
    run([node, tools / 'hvigor/bin/hvigorw.js', '--mode', 'module', '-p', 'product=default',
         'assembleHap', '--no-daemon'], env, project)
    print('HAP directory:', project / 'entry/build/default/outputs/default')


if __name__ == '__main__':
    main()
