import importlib.machinery
import importlib.util
import os
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

SOURCE = Path(__file__).with_name('uuremote-image-clipboard')
loader = importlib.machinery.SourceFileLoader('uu_image_test', str(SOURCE))
spec = importlib.util.spec_from_loader(loader.name, loader)
image = importlib.util.module_from_spec(spec)
loader.exec_module(image)


class ImageWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.environment = patch.dict(os.environ, {'XDG_RUNTIME_DIR': str(self.root)})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.entry = {'device_id': 'fixture-mac', 'platform': 4, 'image_clipboard': True}
        self.helper = types.SimpleNamespace(
            read_entry=Mock(return_value=(self.entry, 'fixture-app', 'fixture.service')),
            active=Mock(return_value=True), launch=Mock(return_value=0), focus=Mock(),
            viewer_path=Mock(return_value='/fixture/uuremote-viewer'),
        )

    def test_read_failure_never_stops_or_connects_to_remote(self):
        with patch.object(image, 'independent', return_value=self.helper), \
                patch.object(image, 'read_image', side_effect=RuntimeError('no image')), \
                patch.object(image, 'notify'), patch.object(image, 'send_image') as send, \
                patch.object(image.subprocess, 'run') as run, \
                patch.object(image.sys, 'argv', ['image-helper', 'a' * 16]):
            with self.assertRaises(RuntimeError):
                image.main()
            send.assert_not_called()
            run.assert_not_called()
            self.helper.launch.assert_not_called()

    def test_failed_transfer_restores_previously_running_desktop(self):
        with patch.object(image, 'independent', return_value=self.helper), \
                patch.object(image, 'read_image'), patch.object(image, 'notify'), \
                patch.object(image, 'send_image', side_effect=RuntimeError('transfer failed')), \
                patch.object(image.subprocess, 'run') as run, \
                patch.object(image.sys, 'argv', ['image-helper', 'a' * 16]):
            with self.assertRaises(RuntimeError):
                image.main()
            self.assertEqual(run.call_args.args[0], ['systemctl', '--user', 'stop', 'fixture.service'])
            self.helper.launch.assert_called_once_with('a' * 16)

    def test_non_mac_or_disabled_entries_cannot_run_mac_commands(self):
        for platform, enabled in [(1, True), (4, False)]:
            self.entry.update(platform=platform, image_clipboard=enabled)
            with patch.object(image, 'independent', return_value=self.helper), \
                    patch.object(image, 'read_image') as read, \
                    patch.object(image.sys, 'argv', ['image-helper', 'a' * 16]):
                with self.assertRaises(ValueError):
                    image.main()
                read.assert_not_called()

    def test_mac_transfer_prepares_private_directory_and_never_sends_chat(self):
        events = []

        def terminal(device, command, marker, executable):
            events.append(('terminal', command))
            self.assertEqual(device, 'fixture-mac')
            self.assertEqual(executable, '/fixture/uuremote-viewer')
            self.assertNotIn(marker, command, 'A command echo must not look like completion')
            return True

        def upload(arguments, **options):
            events.append(('upload', arguments))
            return types.SimpleNamespace(returncode=0)

        with patch.object(image, 'independent', return_value=self.helper), \
                patch.object(image, 'terminal_command', side_effect=terminal), \
                patch.object(image.subprocess, 'run', side_effect=upload):
            image.send_image(self.entry, self.root / 'fixture.png')
        self.assertEqual([kind for kind, _ in events], ['terminal', 'upload', 'terminal'])
        self.assertIn('/bin/mkdir -m 700', events[0][1])
        self.assertTrue(events[1][1][-1].endswith('/image.png'))
        self.assertIn('NSPasteboard.generalPasteboard', events[2][1])
        self.assertIn('/bin/rm -f', events[2][1])
        self.assertIn('/bin/rmdir', events[2][1])
        self.assertNotIn('System Events', events[2][1])
        self.assertNotIn('keystroke', events[2][1])


if __name__ == '__main__':
    unittest.main()
