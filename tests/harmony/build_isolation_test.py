"""Regression checks for Cargo target and dependency isolation."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('harmony_build', ROOT / 'scripts/build-harmony.py')
BUILD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD)


def metadata(manifest, target):
    result = subprocess.run([
        'cargo', 'metadata', '--locked', '--offline', '--format-version', '1',
        '--filter-platform', target, '--manifest-path', str(manifest),
    ], capture_output=True, text=True, check=True)
    return json.loads(result.stdout)


class BuildIsolationTest(unittest.TestCase):
    def test_real_cargo_targets_and_features_are_isolated(self):
        originals = {name: (ROOT / name).read_bytes() for name in ['Cargo.toml', 'Cargo.lock']}
        with tempfile.TemporaryDirectory(prefix='uu-harmony-build-test-') as directory:
            project = Path(directory)
            manifest = BUILD.prepare_rust_project(project, ROOT / 'harmony/Cargo.lock')
            linux = metadata(ROOT / 'Cargo.toml', 'aarch64-unknown-linux-gnu')
            ohos = metadata(manifest, 'aarch64-unknown-linux-ohos')
            for data, is_ohos in [(linux, False), (ohos, True)]:
                package = next(p for p in data['packages'] if p['name'] == 'uu-remote-linux')
                for target in package['targets']:
                    self.assertTrue(Path(target['src_path']).is_file(), target['name'])
                libraries = [t for t in package['targets'] if t['name'] == 'uu_remote_harmony']
                self.assertEqual(len(libraries), int(is_ohos))
                if libraries:
                    self.assertEqual(libraries[0]['kind'], ['staticlib'])
                util = next(p for p in data['packages'] if p['name'] == 'webrtc-util')
                self.assertEqual(util['version'], '0.8.1')
                self.assertEqual(util['source'] is None, is_ohos)
                nix = next(p for p in data['packages'] if p['name'] == 'nix' and p['version'] == '0.26.4')
                node = next(n for n in data['resolve']['nodes'] if n['id'] == nix['id'])
                self.assertIn('net', node['features'])
                self.assertIn('socket', node['features'])
                self.assertEqual('default' in node['features'], not is_ohos)
                if is_ohos:
                    self.assertTrue({'aio', 'mqueue', 'fs'}.isdisjoint(node['features']))
            self.assertFalse((project / 'harmony/signing.local.json').exists())
        for name, contents in originals.items():
            self.assertEqual((ROOT / name).read_bytes(), contents)

    def test_preparation_is_repeatable(self):
        with tempfile.TemporaryDirectory(prefix='uu-harmony-build-test-') as directory:
            project = Path(directory)
            first = BUILD.prepare_rust_project(project, ROOT / 'harmony/Cargo.lock').read_bytes()
            second = BUILD.prepare_rust_project(project, ROOT / 'harmony/Cargo.lock').read_bytes()
            self.assertEqual(first, second)
            self.assertTrue((project / 'src').samefile(ROOT / 'src'))

    def test_existing_files_are_not_overwritten(self):
        with tempfile.TemporaryDirectory(prefix='uu-harmony-build-test-') as directory:
            project = Path(directory)
            (project / 'src').write_text('existing data')
            with self.assertRaises(RuntimeError):
                BUILD.prepare_rust_project(project, ROOT / 'harmony/Cargo.lock')
            self.assertEqual((project / 'src').read_text(), 'existing data')


if __name__ == '__main__':
    unittest.main()
