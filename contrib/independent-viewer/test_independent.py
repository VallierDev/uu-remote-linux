import importlib.machinery
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SOURCE = Path(__file__).with_name('uuremote-independent')
loader = importlib.machinery.SourceFileLoader('uu_independent_test', str(SOURCE))
spec = importlib.util.spec_from_loader(loader.name, loader)
helper = importlib.util.module_from_spec(spec)
loader.exec_module(helper)


class IndependentEntryTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.executable = self.root / 'path with spaces' / 'uuremote-viewer'
        self.executable.parent.mkdir()
        self.executable.write_text('#!/bin/sh\nexit 0\n')
        self.executable.chmod(0o755)
        self.environment = patch.dict(os.environ, {
            'XDG_CONFIG_HOME': str(self.root / 'config'),
            'XDG_DATA_HOME': str(self.root / 'data'),
            'UU_VIEWER_EXECUTABLE': str(self.executable),
        })
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.font = patch.object(helper, 'default_font', return_value='')
        self.font.start()
        self.addCleanup(self.font.stop)

    def test_device_entry_is_private_and_round_trips_without_credentials(self):
        key, entry = helper.setup('fixture-device-one', '测试 Mac', 4)
        path, app_id, unit = helper.paths(key)
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(path.parent.stat().st_mode & 0o777, 0o700)
        self.assertEqual(helper.read_entry(key)[0], entry)
        self.assertEqual(entry['viewer'], str(self.executable))
        self.assertNotIn('token', entry)
        self.assertEqual(app_id, 'uu-remote-linux-session-' + key)
        self.assertEqual(unit, 'uuremote-independent-' + key + '.service')

    def test_preferences_survive_reopening_and_only_one_desktop_entry_is_created(self):
        key, entry = helper.setup('fixture-device-one', 'Mac', 4)
        entry.update(force_join=True, UU_VIEWER_SIZE="1920x1080")
        helper.save_entry(key, entry)
        again, reopened = helper.setup('fixture-device-one', 'Mac renamed', 4)
        self.assertEqual(key, again)
        self.assertTrue(reopened['force_join'])
        self.assertEqual(reopened['UU_VIEWER_SIZE'], '1920x1080')
        desktops = list((self.root / 'data/applications').glob('*.desktop'))
        self.assertEqual(len(desktops), 1)
        self.assertNotIn('[Desktop Action', desktops[0].read_text())

    def test_names_cannot_inject_a_desktop_action(self):
        key, entry = helper.setup('fixture-device-two', 'name\nExec=bad\r\n[Desktop Action bad]', 4)
        text = (self.root / 'data/applications' / (helper.paths(key)[1] + '.desktop')).read_text()
        self.assertNotIn('\nExec=bad', text)
        self.assertNotIn('\n[Desktop Action bad]', text)
        self.assertNotIn('\n', entry['name'])


    def test_world_readable_or_symlinked_configuration_is_rejected(self):
        key, _ = helper.setup('fixture-device-one', 'Mac', 4)
        path, _, _ = helper.paths(key)
        path.chmod(0o644)
        with self.assertRaises(ValueError):
            helper.read_entry(key)
        other = path.with_suffix('.other')
        path.rename(other)
        path.symlink_to(other)
        with self.assertRaises(ValueError):
            helper.read_entry(key)

    def test_invalid_key_cannot_escape_config_directory(self):
        for key in ['../config', '', 'g' * 16, 'a' * 17]:
            with self.assertRaises(ValueError):
                helper.paths(key)

    def test_existing_active_session_is_focused_instead_of_reconnected(self):
        key, _ = helper.setup('fixture-device-one', 'Mac', 4)
        with patch.object(helper, 'active', return_value=True), \
                patch.object(helper, 'focus', return_value=True) as focus, \
                patch.object(helper.subprocess, 'run') as run:
            self.assertEqual(helper.launch(key), 0)
            focus.assert_called_once_with(helper.paths(key)[1], helper.paths(key)[2])
            run.assert_not_called()


    def test_service_receives_custom_config_and_language(self):
        key, _ = helper.setup('fixture-device-one', 'Mac', 4)
        with patch.dict(os.environ, {'UU_CONFIG': '/fixture/config.json', 'UU_LANG': 'en'}), \
                patch.object(helper, 'active', return_value=False), \
                patch.object(helper.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
            self.assertEqual(helper.launch(key), 0)
        arguments = run.call_args.args[0]
        for name in ['XDG_CONFIG_HOME', 'XDG_DATA_HOME']:
            self.assertIn('--setenv=' + name + '=' + os.environ[name], arguments)
        self.assertIn('--setenv=UU_CONFIG=/fixture/config.json', arguments)
        self.assertIn('--setenv=UU_LANG=en', arguments)

    def test_connect_can_take_over_and_keeps_the_selected_device(self):
        first, _ = helper.setup('fixture-device-one', 'Mac', 4)
        second, entry = helper.setup('fixture-device-two', 'PC', 1)
        for key, device in [(first, 'fixture-device-one'), (second, 'fixture-device-two')]:
            with patch.object(helper.os, 'execve') as execute, \
                    patch.object(helper.Path, 'home', return_value=self.root), \
                    patch.object(helper.os, 'umask'):
                helper.run(key)
            executable, args, environment = execute.call_args.args
            self.assertEqual(executable, str(self.executable))
            self.assertEqual(args[1:], ['connect', device, 'false', '-', 'echo', 'legacy'])
        entry['force_join'] = True
        helper.save_entry(second, entry)
        with patch.object(helper.os, 'execve') as execute, patch.object(helper.os, 'umask'):
            helper.run(second)
        self.assertEqual(execute.call_args.args[1][3], 'true')




if __name__ == '__main__':
    unittest.main()
