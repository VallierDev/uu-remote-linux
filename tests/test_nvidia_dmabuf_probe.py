import contextlib
import importlib.util
import io
import pathlib
import tempfile
import unittest
from unittest import mock

path = pathlib.Path(__file__).resolve().parents[1] / 'contrib/nvidia-dmabuf-probe.py'
spec = importlib.util.spec_from_file_location('probe', path)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class ProbeTests(unittest.TestCase):
    def test_pci_normalization_and_invalid(self):
        self.assertEqual(probe.pci_key('00000000:26:00.0'), probe.pci_key('0000:26:00.0'))
        self.assertIsNone(probe.pci_key('not-pci'))

    def test_main_query_statuses(self):
        def run(count=1, device_rc=0, pci_rc=0, pci=b'0000:26:00.0', nvenc_rc=0, missing_nvenc=False):
            cuda = mock.Mock()
            cuda.cuInit.return_value = 0
            cuda.cuDeviceGetCount.side_effect = lambda p: setattr(p._obj, 'value', count) or 0
            cuda.cuDeviceGet.return_value = device_rc
            cuda.cuDeviceGetPCIBusId.side_effect = lambda buf, size, dev: setattr(buf, 'value', pci) or pci_rc
            nvenc = None if missing_nvenc else mock.Mock()
            if nvenc is not None:
                nvenc.NvEncodeAPIGetMaxSupportedVersion.return_value = nvenc_rc
            output = io.StringIO()
            with mock.patch.object(probe, 'load', side_effect=[cuda, nvenc]), \
                 mock.patch.object(probe, 'render_nodes', return_value=['/dev/dri/renderD128']), \
                 contextlib.redirect_stdout(output):
                rc = probe.main()
            return rc, output.getvalue()
        rc, output = run()
        self.assertEqual(rc, 0)
        self.assertIn('DMA-BUF was not verified', output)
        for args in ({'count': 0}, {'device_rc': 1}, {'pci_rc': 1},
                     {'pci': b'invalid'}, {'nvenc_rc': 1}, {'missing_nvenc': True}):
            with self.subTest(args=args):
                self.assertNotEqual(run(**args)[0], 0)

    def test_only_matching_render_nodes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            for name, pci in [('renderD128', '0000:26:00.0'), ('renderD129', '0000:27:00.0')]:
                node = root / name
                node.mkdir()
                (node / 'device').symlink_to(root / pci)
            self.assertEqual(probe.render_nodes(probe.pci_key('00000000:26:00.0'), tmp), ['/dev/dri/renderD128'])
            self.assertEqual(probe.render_nodes(None, tmp), [])


if __name__ == '__main__':
    unittest.main()
