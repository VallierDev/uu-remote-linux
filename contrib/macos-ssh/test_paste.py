"""Synthetic fixtures only; never inspect or change the general pasteboard."""

import base64
import errno
import hashlib
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("uu-ssh-paste")
loader = importlib.machinery.SourceFileLoader("uu_paste", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
paste = importlib.util.module_from_spec(spec)
loader.exec_module(paste)


class FramingTests(unittest.TestCase):
    def test_paste_at_every_fragment_boundary_and_multiple_pastes(self):
        wire = b"key" + paste.START + b"hello\nworld" + paste.END + b"\r"
        expected = [("data", b"key"), ("paste", b"hello\nworld"), ("data", b"\r")]
        for split in range(len(wire) + 1):
            frames = paste.InputFrames()
            events = frames.feed(wire[:split]) + frames.feed(wire[split:])
            merged = []
            for kind, data in events:
                if kind == "data" and merged and merged[-1][0] == "data":
                    merged[-1] = (kind, merged[-1][1] + data)
                else:
                    merged.append((kind, data))
            self.assertEqual(merged, expected)
        frames = paste.InputFrames()
        self.assertEqual(frames.feed(paste.START + b"a" + paste.END + paste.START + b"b" + paste.END),
                         [("paste", b"a"), ("paste", b"b")])

    def test_mouse_and_arrow_bytes_and_standalone_escape(self):
        raw = b"\x1b[<2;12;4M\x1b[<2;12;4m\x1b[<35;13;5M\x1b[A\x1b"
        frames = paste.InputFrames()
        output = b"".join(data for kind, data in frames.feed(raw) if kind == "data")
        self.assertEqual(output + frames.flush_key(), raw)

    def test_oversized_paste_is_discarded_and_parser_recovers(self):
        frames = paste.InputFrames()
        with mock.patch.object(paste, "MAX_PASTE", 16):
            self.assertEqual(frames.feed(paste.START + b"x" * 40), [])
            self.assertEqual(frames.feed(paste.END + b"next")[-1], ("data", b"next"))
            self.assertFalse(frames.pasting)
            self.assertLess(len(frames.buffer), 16)

    def test_remote_modes_track_split_sequences_without_disabling_local_framing(self):
        for split in range(len(paste.DISABLE) + 1):
            modes = paste.OutputModes()
            modes.bracketed = True
            result = modes.feed(paste.DISABLE[:split]) + modes.feed(paste.DISABLE[split:])
            self.assertEqual(result, paste.ENABLE)
            self.assertFalse(modes.bracketed)
        modes = paste.OutputModes()
        self.assertEqual(modes.feed(b"\x1b[?1;2004h"), b"\x1b[?1h" + paste.ENABLE)
        self.assertTrue(modes.bracketed)
        self.assertEqual(modes.feed(b"\x1b[?2004;25l"), b"\x1b[?25l" + paste.ENABLE)
        self.assertFalse(modes.bracketed)
        self.assertEqual(modes.feed(b"\x1bc"), b"\x1bc" + paste.ENABLE)
        raw = b"\x1b[31mhello\x1b[0m\x1b]0;title\x07"
        self.assertEqual(modes.feed(raw), raw)


class FileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.files = [self.root / "space 中文.png", self.root / "quote'$(literal);.txt"]
        for path in self.files:
            path.write_bytes(os.urandom(1025))
        self.paths = [str(p) for p in self.files]
        # Ghostty uses backslash escaping rather than POSIX single quotes.
        escape = "\\ ()[]{}<>\"'`!#$&;|*?\t"
        self.payload = " ".join("".join("\\" + c if c in escape else c for c in p)
                                for p in self.paths).encode()

    def test_finder_files_match_ghostty_escaping(self):
        self.assertEqual(paste.copied_files(self.payload, lambda: self.paths), self.paths)

    def test_plain_text_and_pasted_commands_do_not_upload(self):
        clipboard = mock.Mock(return_value=[])
        for value in (b"hello\nworld", b"cat /tmp/file", b"\xff", b"'unterminated"):
            self.assertIsNone(paste.copied_files(value, clipboard))
        clipboard.assert_not_called()
        self.assertIsNone(paste.copied_files(self.payload, lambda: []))
        self.assertIsNone(paste.copied_files(self.payload, lambda: self.paths[::-1]))

    def test_missing_files_folders_duplicate_names_and_control_characters_rejected(self):
        cases = [[str(self.root)], [str(self.root / "missing")]]
        newline = self.root / "bad\nname"
        newline.write_text("synthetic")
        cases.append([str(newline)])
        folder = self.root / "another"
        folder.mkdir()
        duplicate = folder / self.files[0].name
        duplicate.write_text("synthetic")
        cases.append([self.paths[0], str(duplicate)])
        for paths in cases:
            with self.assertRaises(paste.PasteError):
                paste.copied_files(shlex.join(paths).encode(), lambda: paths)

    def test_upload_paths_are_quoted_and_failed_batch_is_cleaned(self):
        uploader = paste.Uploader(self.root / "config with spaces", "uu-test")
        directory = b"/home/test user/.local/share/uuremote/uploads/paste.ABCD1234\n"
        with mock.patch.object(paste, "run_private", side_effect=[directory, b""]) as run:
            result = uploader.upload(self.paths, threading.Event())
        self.assertEqual(shlex.split(result.decode()),
                         [directory.decode().strip() + "/" + p.name for p in self.files])
        self.assertEqual(run.call_args_list[1].args[0][-3:-1], self.paths)
        self.assertNotIn("-O", run.call_args_list[1].args[0])
        self.assertIn("BatchMode=yes", run.call_args_list[1].args[0])
        with mock.patch.object(paste, "run_private", side_effect=[directory, paste.PasteError("failed"), b""]) as run:
            with self.assertRaises(paste.PasteError):
                uploader.upload(self.paths, threading.Event())
        cleanup = run.call_args_list[2].args[0][-1]
        self.assertEqual(shlex.split(cleanup), ["rm", "-rf", "--", directory.decode().strip()])

    def test_bad_destination_and_bad_clipboard_fail_without_content_in_error(self):
        uploader = paste.Uploader(None, "uu-test")
        for output in (b"relative", b"/tmp/invalid", b"/tmp/paste.abc\nextra", b"\xff"):
            with mock.patch.object(paste, "run_private", return_value=output):
                with self.assertRaises(paste.PasteError):
                    uploader.upload(self.paths, threading.Event())
        with mock.patch.object(paste.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"{}")):
            with self.assertRaises(paste.PasteError):
                paste.clipboard_paths("helper")

    def test_cancellation_stops_transfer(self):
        cancel = threading.Event()
        cancel.set()
        with self.assertRaisesRegex(paste.PasteError, "cancelled"):
            paste.run_private([sys.executable, "-c", "import time;time.sleep(30)"], cancel, 3)


class ProgressTests(unittest.TestCase):
    def test_numeric_meter_handles_fragments_multiple_files_and_rounding(self):
        for fragment in (1, 7, 9999):
            reports = []
            meter = paste.SCPMeter([1048576, 1025], lambda *args: reports.append(args))
            wire = (b"\rprivate-file  0%    0    0.0KB/s   --:-- ETA"
                    b"\rprivate-file 50%  512KB 128.0KB/s   00:04 ETA"
                    b"\rprivate-file 100% 1024KB 128.0KB/s   00:08\r\n"
                    b"\rsecret-other  0%    0   0.0KB/s   --:-- ETA"
                    b"\rsecret-other 100% 1025  64.0KB/s   00:00\r\n")
            for offset in range(0, len(wire), fragment):
                meter.feed(wire[offset:offset + fragment])
            self.assertIn(("uploading", 524288, 1049601, 131072), reports)
            self.assertEqual(reports[-1], ("uploading", 1049601, 1049601, 65536))
            self.assertEqual(meter.index, 2)
            self.assertEqual([p[1] for p in reports], sorted(p[1] for p in reports))
            self.assertNotIn("private-file", repr(reports))
            self.assertNotIn("secret-other", repr(reports))

    def test_unrecognized_output_is_not_displayed_and_buffer_is_bounded(self):
        reports = []
        meter = paste.SCPMeter([10], lambda *args: reports.append(args))
        meter.feed(b"private error " * 10000 + b" 999% 5 1.0KB/s 00:00\n")
        self.assertEqual(reports, [])
        self.assertLessEqual(len(meter.buffer), 512)

    def test_metered_process_captures_progress_privately_and_preserves_failure_cancellation(self):
        reports = []
        code = ("import os;assert os.isatty(1);assert os.tcgetpgrp(1)==os.getpgrp();"
                "os.write(1,b'\\rprivate-file 100% 1025 64.0KB/s 00:00\\n')")
        paste.run_metered([sys.executable, "-c", code], threading.Event(), 5,
                          paste.SCPMeter([1025], lambda *args: reports.append(args)))
        self.assertEqual(reports[-1], ("uploading", 1025, 1025, 65536))
        with self.assertRaises(paste.PasteError):
            paste.run_metered([sys.executable, "-c", "raise SystemExit(2)"], threading.Event(), 5,
                              paste.SCPMeter([1], lambda *_: None))
        cancel = threading.Event()
        cancel.set()
        with self.assertRaises(paste.PasteError):
            paste.run_metered([sys.executable, "-c", "import time;time.sleep(30)"], cancel, 5,
                              paste.SCPMeter([1], lambda *_: None))

    def test_display_restores_split_remote_title_and_never_changes_terminal_cells(self):
        output = []
        display = paste.UploadDisplay(output.append)
        for byte in b"\x1b]2;remote app title\x1b\\":
            display.observe(bytes([byte]))
        with mock.patch.object(paste.time, "monotonic", return_value=10):
            display.start(1048576)
        with mock.patch.object(paste.time, "monotonic", return_value=13):
            display.update("uploading", 524288, 1048576, 131072)
            display.render()
            display.render()  # Throttled, not a second display update.
        self.assertEqual(len(output), 2)
        self.assertIn(b"Connecting", output[0])
        self.assertIn(b"Uploading 50% | 512.0 KiB/1.0 MiB | 128.0 KiB/s | 3s", output[1])
        self.assertIn(b"\x1b]9;4;1;50\x1b\\", output[1])
        display.observe(b"\x1b]0;new remote title\x07")
        display.finish()
        self.assertEqual(output[-1], b"\x1b]9;4;0\x1b\\\x1b]2;new remote title\x1b\\")
        display.finish()
        self.assertEqual(len(output), 3)
        self.assertTrue(all(b"\n" not in item and b"\r" not in item for item in output))


HARNESS = r'''
import os, runpy, sys, time, json
h = runpy.run_path(sys.argv[1])
paths, remote, mode, fail = json.loads(sys.argv[2])
class Upload:
    def upload(self, files, cancel, progress):
        assert files == paths
        progress('uploading',512,1024,256)
        time.sleep(0.1)
        if fail == 'cancel':
            assert cancel.wait(5)
        if fail:
            raise h['PasteError']('synthetic failure')
        return remote.encode()
child = """import os,tty,base64
tty.setraw(0)
os.write(1, %r + b'CHILD_READY')
data=b''
while not data.endswith(b'\\r'):
    part=os.read(0,65536)
    if not part: break
    data+=part
os.write(1,b'RECEIVED:' + base64.b64encode(data) + b':DONE')
raise SystemExit(7)
""" % (h['ENABLE'] if mode else h['DISABLE'])
raise SystemExit(h['relay']([sys.executable,'-c',child], lambda: paths, Upload()))
'''


class RelayTests(unittest.TestCase):
    def test_resize_and_signal_shutdown_restore_terminal(self):
        child = r'''
import fcntl,os,signal,struct,termios,tty
tty.setraw(0)
def resized(*_):
    rows,cols,_,_=struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,bytes(8)))
    os.write(1,('SIZE:%d,%d;'%(rows,cols)).encode())
signal.signal(signal.SIGWINCH,resized)
resized()
while True: signal.pause()
'''
        harness = "import runpy,sys;h=runpy.run_path(sys.argv[1]);h['relay']([sys.executable,'-c',sys.argv[2]],lambda:[],None)"
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        import fcntl
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        proc = subprocess.Popen([sys.executable, "-c", harness, str(SCRIPT), child],
                                stdin=slave, stdout=slave, stderr=slave)
        output = bytearray()

        def until(marker):
            deadline = time.monotonic() + 5
            while marker not in output:
                self.assertLess(time.monotonic(), deadline, "Resize was not forwarded")
                if select.select([master], [], [], 0.1)[0]:
                    output.extend(os.read(master, 65536))
        try:
            until(b"SIZE:24,80;")
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 31, 110, 0, 0))
            proc.send_signal(signal.SIGWINCH)
            until(b"SIZE:31,110;")
            proc.send_signal(signal.SIGTERM)
            self.assertEqual(proc.wait(timeout=5), 128 + signal.SIGTERM)
            restored = termios.tcgetattr(slave)
            restored[3] &= ~getattr(termios, "PENDIN", 0)
            original[3] &= ~getattr(termios, "PENDIN", 0)
            self.assertEqual(restored, original)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            os.close(master)
            os.close(slave)

    def test_real_pty_upload_order_mouse_text_failure_exit_status_and_restoration(self):
        with tempfile.TemporaryDirectory() as tmp:
            file = Path(tmp) / "paste space 中文.txt"
            file.write_text("synthetic fixture")
            mouse = b"\x1b[<2;2;3M\x1b[<2;2;3m\x1b[<35;3;4M"
            for mode, fail, ordinary in ((False, False, False), (True, False, False),
                                         (True, True, False), (False, False, True),
                                         (True, False, True), (True, 'cancel', False)):
                with self.subTest(mode=mode, fail=fail, ordinary=ordinary):
                    master, slave = pty.openpty()
                    original = termios.tcgetattr(slave)
                    remote = shlex.quote("/home/test/.local/share/uuremote/uploads/paste.test/space 中文.txt")
                    config = json.dumps([[str(file)], remote, mode, fail])
                    proc = subprocess.Popen([sys.executable, "-c", HARNESS, str(SCRIPT), config],
                                            stdin=slave, stdout=slave, stderr=slave)
                    output = bytearray()
                    try:
                        deadline = time.monotonic() + 10
                        while b"CHILD_READY" not in output:
                            self.assertLess(time.monotonic(), deadline, "PTY harness did not start")
                            if select.select([master], [], [], 0.1)[0]:
                                output.extend(os.read(master, 65536))
                        payload = b"ordinary\ntext" if ordinary else shlex.quote(str(file)).encode()
                        wire = mouse + paste.START + payload + paste.END
                        wire += b"\x03\r" if fail == 'cancel' else b"\r"
                        for byte in wire:
                            os.write(master, bytes([byte]))
                        while proc.poll() is None or select.select([master], [], [], 0)[0]:
                            self.assertLess(time.monotonic(), deadline, "PTY relay did not exit")
                            if select.select([master], [], [], 0.1)[0]:
                                try:
                                    chunk = os.read(master, 65536)
                                except OSError as error:
                                    if error.errno == errno.EIO:
                                        break
                                    raise
                                if not chunk:
                                    break
                                output.extend(chunk)
                        self.assertEqual(proc.wait(timeout=2), 7)
                        encoded = bytes(output).split(b"RECEIVED:", 1)[1].split(b":DONE", 1)[0]
                        expected = b"" if fail else paste.paste_bytes(payload if ordinary else remote.encode(), mode)
                        interrupt = b"\x03" if fail == 'cancel' else b""
                        self.assertEqual(base64.b64decode(encoded), mouse + expected + interrupt + b"\r")
                        restored = termios.tcgetattr(slave)
                        # macOS sets PENDIN when restoring canonical input. It
                        # is kernel bookkeeping, not an unrestored user mode.
                        restored[3] &= ~getattr(termios, "PENDIN", 0)
                        original[3] &= ~getattr(termios, "PENDIN", 0)
                        self.assertEqual(restored, original)
                        if fail:
                            self.assertNotIn(str(file).encode(), output)
                        if not ordinary:
                            self.assertIn(b"UU upload: Connecting", output)
                            self.assertIn(b"\x1b]9;4;0\x1b\\", output)
                        if fail == 'cancel':
                            self.assertIn(b"UU upload: Cancelling", output)
                    finally:
                        if proc.poll() is None:
                            proc.kill()
                            proc.wait()
                        os.close(master)
                        os.close(slave)


@unittest.skipUnless(os.environ.get("UU_PASTE_TEST_HOST"), "set UU_PASTE_TEST_HOST for a real mapped SSH test")
class MappedSSHTests(unittest.TestCase):
    def test_live_upload_reports_intermediate_progress_and_verified_completion(self):
        host = os.environ["UU_PASTE_TEST_HOST"]
        uploader = paste.Uploader(os.environ.get("UU_PASTE_TEST_CONFIG"), host)
        uploader.scp.extend(["-l", "1024"])  # Deliberately slow a synthetic transfer.
        reports = []
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "private progress 中文.bin"
            source.write_bytes(os.urandom(1024 * 1024))
            paths = shlex.split(uploader.upload([str(source)], threading.Event(),
                                               lambda *args: reports.append(args)).decode())
            try:
                total = source.stat().st_size
                self.assertEqual(reports[0], ("connecting", 0, total, 0))
                self.assertTrue(any(0 < done < total and speed > 0
                                    for stage, done, size, speed in reports if stage == "uploading"))
                self.assertEqual(reports[-1], ("finishing", total, total, 0))
                self.assertNotIn(source.name, repr(reports))
                remote = paste.run_private([*uploader.ssh, "sha256sum -- " + shlex.quote(paths[0])],
                                           threading.Event(), 30)
                self.assertEqual(remote.split()[0].decode(), hashlib.sha256(source.read_bytes()).hexdigest())
            finally:
                paste.run_private([*uploader.ssh, "rm -rf -- " + shlex.quote(str(Path(paths[0]).parent))],
                                  threading.Event(), 30)

    def test_paste_through_real_pty_and_uu_ssh_opens_uploaded_files(self):
        host = os.environ["UU_PASTE_TEST_HOST"]
        config = os.environ.get("UU_PASTE_TEST_CONFIG")
        with tempfile.TemporaryDirectory() as tmp:
            files = [Path(tmp) / "copied file 中文.bin", Path(tmp) / "literal'$(echo nope);.bin"]
            for file in files:
                file.write_bytes(os.urandom(65569))
            hashes = [hashlib.sha256(f.read_bytes()).hexdigest() for f in files]
            mouse = b"\x1b[<2;2;3M\x1b[<2;2;3m\x1b[<35;3;4M"
            remote = r'''
import os,tty,shlex,hashlib,json,shutil
from pathlib import Path
tty.setraw(0)
os.write(1,b'\x1b[?2004hLIVE_READY')
data=b''
while not data.endswith(b'\r'):
    data+=os.read(0,65536)
mouse=b'\x1b[<2;2;3M\x1b[<2;2;3m\x1b[<35;3;4M'
assert data.startswith(mouse+b'\x1b[200~') and data.endswith(b'\x1b[201~\r')
paths=shlex.split(data[len(mouse)+6:-7].decode())
assert len(paths)==2
hashes=[hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in paths]
for directory in {Path(p).parent for p in paths}:
    assert directory.parent==Path.home()/'.local/share/uuremote/uploads'
    assert directory.name.startswith('paste.')
    shutil.rmtree(directory)
os.write(1,b'LIVE_HASHES:'+json.dumps(hashes).encode()+b':DONE')
'''
            harness = r'''
import runpy,sys,json
h=runpy.run_path(sys.argv[1])
host,config,paths,remote=json.loads(sys.argv[2])
uploader=h['Uploader'](config,host)
common=['-F',config] if config else []
raise SystemExit(h['relay'](['/usr/bin/ssh',*common,'-tt',host,remote],lambda:paths,uploader))
'''
            args = json.dumps([host, config, [str(p) for p in files], "python3 -c " + shlex.quote(remote)])
            master, slave = pty.openpty()
            process = subprocess.Popen([sys.executable, "-c", harness, str(SCRIPT), args],
                                       stdin=slave, stdout=slave, stderr=slave)
            output = bytearray()
            try:
                deadline = time.monotonic() + 60
                sent = False
                while process.poll() is None or select.select([master], [], [], 0)[0]:
                    self.assertLess(time.monotonic(), deadline, "Live paste timed out")
                    if select.select([master], [], [], 0.1)[0]:
                        output.extend(os.read(master, 65536))
                    if b"LIVE_READY" in output and not sent:
                        payload = shlex.join([str(p) for p in files]).encode()
                        os.write(master, mouse + paste.START + payload + paste.END + b"\r")
                        sent = True
                self.assertEqual(process.wait(timeout=2), 0)
                received = bytes(output).split(b"LIVE_HASHES:", 1)[1].split(b":DONE", 1)[0]
                self.assertEqual(json.loads(received), hashes)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                os.close(master)
                os.close(slave)

    def test_real_upload_roundtrip_binary_unicode_and_shell_metacharacters(self):
        host = os.environ["UU_PASTE_TEST_HOST"]
        config = os.environ.get("UU_PASTE_TEST_CONFIG")
        uploader = paste.Uploader(config, host)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            files = [root / "space 中文.bin", root / "literal'$(echo nope);.bin"]
            for file in files:
                file.write_bytes(os.urandom(65569))
            paths = shlex.split(uploader.upload([str(f) for f in files], threading.Event()).decode())
            try:
                for index, (source, remote) in enumerate(zip(files, paths)):
                    dest = root / ("download-" + str(index))
                    paste.run_private([*uploader.scp, f"{host}:{remote}", str(dest)], threading.Event(), 60)
                    self.assertEqual(hashlib.sha256(source.read_bytes()).digest(),
                                     hashlib.sha256(dest.read_bytes()).digest())
                directory = str(Path(paths[0]).parent)
                mode = paste.run_private([*uploader.ssh, "stat -c %a -- " + shlex.quote(directory)],
                                         threading.Event(), 30)
                self.assertEqual(mode.strip(), b"700")
            finally:
                paste.run_private([*uploader.ssh, "rm -rf -- " + shlex.quote(str(Path(paths[0]).parent))],
                                  threading.Event(), 30)


if __name__ == "__main__":
    unittest.main()
