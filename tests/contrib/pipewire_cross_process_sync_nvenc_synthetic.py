#!/usr/bin/env python3
"""Cross-process synthetic EGL/CUDA/NVENC and native-fence timeline test, not Portal safety.

Only self-created BOs are used. Exit or timeout does not isolate GPU access.
Positive and --negative-mismatch success both exit 0;
distinguish them by the command line and explicit PASS marker. The negative
mode injects a failure result after successful encoding to test control flow;
it does not make an actual comparison fail or cover decoded pixel mismatch.
--disconnect-after exercises real stream removal between completed frames,
requires both child clients to exit 1 and explicit idle encoder cleanup, and
exits 0 only after verifying that no subsequent frame was processed.
--disconnect-worker removes the stream at a worker checkpoint after CUDA copy
returns and before explicit synchronization; no claim that hardware remains busy.
The worker drains into a local test bitstream, with no release, queue or ACK.

Trust boundary: root and same-UID processes are trusted not to replace paths;
filesystem ACLs and the mount namespace must not grant other users write access
or path replacement. CC, CFLAGS, LDFLAGS, PATH and LD_* are trusted test inputs.
Path checks are not cryptographic authentication of the daemon or an adversarial
same-UID/environment test.

Cleanup manages and reaps only direct children; tools such as CC may spawn
unmanaged descendants. No process-group cleanup or complete cleanup after
abnormal termination is guaranteed. The positive path requires the producer to
dequeue the original buffer, validate its consumer-written generation and observe
the release point for every frame. --frames reuses one DMA-BUF and encoder;
local PipeWire loops stay running while a loop-thread barrier pauses frame
processing between generations. Stream lifecycle events remain active.
"""
import argparse
import os
import pathlib
import re
import secrets
import shlex
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
from pipewire_synthetic_smoke import CONFIG, command, spawn, terminate


LOG_LIMIT = 128 * 1024


def read_client_log(temporary, log):
    temporary.check()
    st = os.fstat(log.fileno())
    if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid():
        raise RuntimeError('invalid synthetic client log')
    data = os.pread(log.fileno(), LOG_LIMIT, 0)
    if len(data) != min(st.st_size, LOG_LIMIT):
        raise RuntimeError('synthetic client log changed or could not be read completely')
    return data.decode('utf-8', errors='replace'), st.st_size > LOG_LIMIT


def identity(st):
    return st.st_dev, st.st_ino


def trusted_directory(path, owner, forbidden):
    st = os.lstat(path)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != owner or st.st_mode & forbidden:
        raise RuntimeError(f'untrusted runtime directory: {path}')
    return st


class PrivateDirectory(tempfile.TemporaryDirectory):
    # If a child cannot be reaped, leave its runtime directory intact.
    preserve = False

    def __init__(self):
        self.fd = self.socket_fd = -1
        self.parent = pathlib.Path('/run/user') / str(os.getuid())
        trusted_directory('/', 0, 0o022)
        trusted_directory('/run', 0, 0o022)
        trusted_directory('/run/user', 0, 0o022)
        self.parent_identity = identity(trusted_directory(self.parent, os.getuid(), 0o077))
        super().__init__(prefix='uu-pw-cross-', dir=str(self.parent))
        try:
            self.fd = os.open(self.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
            self.check()
        except BaseException:
            self.cleanup()
            raise

    def check(self, require_socket=False):
        trusted_directory('/', 0, 0o022)
        trusted_directory('/run', 0, 0o022)
        trusted_directory('/run/user', 0, 0o022)
        if identity(trusted_directory(self.parent, os.getuid(), 0o077)) != self.parent_identity:
            raise RuntimeError('runtime parent identity changed')
        path_st = trusted_directory(self.name, os.getuid(), 0o077)
        fd_st = os.fstat(self.fd)
        if not stat.S_ISDIR(fd_st.st_mode) or identity(path_st) != identity(fd_st):
            raise RuntimeError('private runtime directory identity changed')
        if require_socket:
            if self.socket_fd < 0:
                raise RuntimeError('daemon socket not pinned')
            sock = os.lstat(pathlib.Path(self.name) / 'pipewire-0')
            if (not stat.S_ISSOCK(sock.st_mode) or sock.st_uid != os.getuid() or
                    identity(sock) != identity(os.fstat(self.socket_fd))):
                raise RuntimeError('private daemon socket identity changed')

    def pin_socket(self):
        self.check()
        path = pathlib.Path(self.name) / 'pipewire-0'
        st = os.lstat(path)
        if not stat.S_ISSOCK(st.st_mode) or st.st_uid != os.getuid():
            raise RuntimeError('invalid private daemon socket')
        try:
            self.socket_fd = os.open(path, os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC)
            if identity(st) != identity(os.fstat(self.socket_fd)):
                raise RuntimeError('daemon socket changed while pinning')
            self.check(require_socket=True)
        except FileNotFoundError as exc:
            raise RuntimeError('daemon socket disappeared while pinning') from exc

    def cleanup(self):
        check_error = None
        if self.fd >= 0 and not self.preserve:
            try:
                self.check()
            except Exception as exc:
                self.preserve = True
                check_error = exc
        for attr in ('socket_fd', 'fd'):
            fd = getattr(self, attr, -1)
            if fd >= 0:
                try:
                    os.close(fd)
                except OSError as exc:
                    self.preserve = True
                    if check_error is None:
                        check_error = exc
                finally:
                    setattr(self, attr, -1)
        if self.preserve:
            self._finalizer.detach()
            print(f'private directory retained: {self.name}', file=sys.stderr)
        else:
            super().cleanup()
        if check_error:
            raise RuntimeError('runtime directory validation or fd cleanup failed') from check_error


def main(device, negative=False, frames=1, disconnect_after=0, disconnect_worker=False, portal_engine=False, native_encoder=False, native_egl=False, flagless=False):
    completed_frames = disconnect_after or frames
    signals = {signal.SIGINT, signal.SIGTERM}
    mask = signal.pthread_sigmask(signal.SIG_BLOCK, set())
    handlers = {s: signal.getsignal(s) for s in signals}
    for s in signals:
        signal.signal(s, terminate)
    children = []
    try:
        temporary = PrivateDirectory()
        with temporary as directory:
            daemon = None
            daemon_log_fd = -1
            client_log = None
            try:
                root = pathlib.Path(directory)
                config = root / 'private.conf'
                config.write_text(CONFIG)
                binary = root / 'probe'
                temporary.check()
                packages = ['libpipewire-0.3', 'gbm', 'libdrm', 'egl', 'glesv2']
                if portal_engine:
                    packages += ['gio-2.0', 'gio-unix-2.0', 'libei-1.0']
                pkg = spawn(children, ['pkg-config', '--cflags', '--libs', *packages],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                flags, errors = pkg.communicate(timeout=10)
                if pkg.returncode:
                    raise RuntimeError('missing build dependencies: ' + errors.strip())
                temporary.check()
                cc = spawn(children, [*shlex.split(os.environ.get('CC', 'cc')), '-std=gnu11', '-Wall', '-Wextra',
                                      '-Werror', '-O1', '-g', *(['-DUU_TEST_PORTAL_BRIDGE', '-Wno-unused-parameter'] if portal_engine else []), *(['-DUU_TEST_NATIVE_ENCODER'] if native_encoder else []), *(['-DUU_TEST_NATIVE_EGL'] if native_egl else []), *(['-DUU_TEST_FLAGLESS'] if flagless else []), *shlex.split(os.environ.get('CFLAGS', '')),
                                      '-I' + str(pathlib.Path(__file__).resolve().parents[2] / 'vendor/ffnvcodec'),
                                      str(pathlib.Path(__file__).with_suffix('.c')),
                                      str(pathlib.Path(__file__).resolve().parents[2] / 'c_engine/portal_nvenc_egl_attrs.c'),
                                      str(pathlib.Path(__file__).resolve().parents[2] / 'c_engine/portal_nvenc_image.c'),
                                      str(pathlib.Path(__file__).resolve().parents[2] / 'c_engine/portal_nvenc_device.c'),
                                      str(pathlib.Path(__file__).resolve().parents[2] / 'contrib/portal-nvenc-experimental.c'), '-o', str(binary),
                                      *shlex.split(flags), *shlex.split(os.environ.get('LDFLAGS', ''))])
                if cc.wait(timeout=30):
                    raise RuntimeError('compilation failed')
                env = {k: v for k, v in os.environ.items() if not k.startswith(('PIPEWIRE_', 'SPA_', 'PW_'))
                       and k not in ('XDG_RUNTIME_DIR', 'XDG_CONFIG_HOME', 'XDG_CONFIG_DIRS')}
                env['UU_PW_NVENC_FRAMES'] = str(frames)
                env.pop('UU_PW_NVENC_DISCONNECT_WORKER', None)
                if disconnect_worker:
                    env['UU_PW_NVENC_DISCONNECT_WORKER'] = '1'
                env.pop('UU_PW_NVENC_DISCONNECT_AFTER', None)
                if disconnect_after:
                    env['UU_PW_NVENC_DISCONNECT_AFTER'] = str(disconnect_after)
                env.pop('UU_PW_SYNC_CUDA_NEGATIVE', None)
                if negative:
                    env['UU_PW_SYNC_CUDA_NEGATIVE'] = '1'
                env.update(PIPEWIRE_RUNTIME_DIR=directory, XDG_RUNTIME_DIR=directory, XDG_CONFIG_HOME=directory,
                           XDG_CONFIG_DIRS=directory, PIPEWIRE_REMOTE=str(root / 'pipewire-0'))
                with (root / 'daemon.log').open('w+') as log:
                    daemon_log_fd = os.dup(log.fileno())
                    client_log = (root / 'clients.log').open('w+')
                    temporary.check()
                    daemon = spawn(children, ['pipewire', '-c', str(config)], env=env, stdout=log, stderr=log)
                    deadline = time.monotonic() + 5
                    while True:
                        temporary.check()
                        if daemon.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError('private daemon failed to start')
                        try:
                            temporary.pin_socket()
                            break
                        except FileNotFoundError:
                            time.sleep(.05)
                    if daemon.poll() is not None:
                        raise RuntimeError('private daemon exited at readiness')
                    temporary.check(require_socket=True)
                    nonce = secrets.token_hex(16)
                    producer_name = 'uu-cross-' + nonce + '-producer'
                    consumer_name = 'uu-cross-' + nonce + '-consumer'
                    left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
                    try:
                        # Enable before producer can send, so the kernel attaches its credentials.
                        right.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
                        temporary.check(require_socket=True)
                        producer = spawn(children, [str(binary), 'producer', str(left.fileno()), device,
                                                    producer_name, '0', str(root / 'synthetic.h264')], env=env, pass_fds=(left.fileno(),), stderr=client_log)
                        temporary.check(require_socket=True)
                        consumer = spawn(children, [str(binary), 'consumer', str(right.fileno()), device,
                                                    consumer_name, str(producer.pid), str(root / 'synthetic.h264')], env=env,
                                         pass_fds=(right.fileno(),), stderr=client_log)
                    finally:
                        left.close()
                        right.close()
                    deadline = time.monotonic() + 8
                    while time.monotonic() < deadline:
                        if daemon.poll() is not None or producer.poll() is not None or consumer.poll() is not None:
                            raise RuntimeError('daemon or client exited before link')
                        ports = []
                        for direction, name in [('-o', producer_name), ('-i', consumer_name)]:
                            temporary.check(require_socket=True)
                            lines = command(children, env, 'pw-link', '-I', direction).splitlines()
                            matches = []
                            for line in lines:
                                parts = line.strip().split(None, 1)
                                if len(parts) == 2 and parts[1].startswith(name + ':'):
                                    if not re.fullmatch(r'[0-9]+', parts[0]):
                                        raise RuntimeError('invalid port ID')
                                    matches.append(parts[0])
                            if len(matches) > 1:
                                raise RuntimeError('ambiguous port')
                            ports.append(matches[0] if matches else None)
                        if all(ports) and ports[0] != ports[1]:
                            temporary.check(require_socket=True)
                            command(children, env, 'pw-link', *ports)
                            break
                        time.sleep(.05)
                    else:
                        raise TimeoutError('video ports not found')
                    deadline = time.monotonic() + 18
                    while producer.poll() is None or consumer.poll() is None:
                        if daemon.poll() is not None:
                            raise RuntimeError('private daemon exited')
                        accepted = (None, 0, 1) if disconnect_after or disconnect_worker else (None, 0)
                        if producer.poll() not in accepted or consumer.poll() not in accepted:
                            raise RuntimeError('client failed')
                        if time.monotonic() > deadline:
                            raise TimeoutError('clients timed out')
                        time.sleep(.01)
                    expected_exit = 1 if disconnect_after or disconnect_worker else 0
                    if producer.returncode != expected_exit or consumer.returncode != expected_exit or daemon.poll() is not None:
                        raise RuntimeError('client or daemon failed')
                    client_errors, truncated = read_client_log(temporary, client_log)
                    if truncated:
                        raise RuntimeError('oversized synthetic client log')
                    if any(client_errors.count(marker) != completed_frames for marker in (
                            'NATIVE_FENCE_TRANSFERRED_TO_ACQUIRE',
                            'CONSUMER_ACQUIRE_WAIT_COMPLETED')):
                        raise RuntimeError('native fence handoff not verified')
                    if any(client_errors.count(marker) != 2 for marker in ('PIPEWIRE_LOOP_STARTED', 'PIPEWIRE_LOOP_STOPPED')):
                        raise RuntimeError('PipeWire loops did not run continuously')
                    if client_errors.count('NATIVE_EGL_DEVICE_INITIALIZED') != 1:
                        raise RuntimeError('native EGL device initialization missing')
                    returned = 'PRODUCER_RETURNED_ORIGINAL_BUFFER_GENERATION_RELEASE'
                    if client_errors.count(returned) != (0 if negative or disconnect_worker else completed_frames):
                        raise RuntimeError('unexpected producer buffer return result')
                    if client_errors.count('synthetic NVENC initialization begin') != 1 or client_errors.count('PERSISTENT_ENCODER_CLEANUP_COMPLETED') != 1:
                        raise RuntimeError('persistent encoder initialization/cleanup not verified')
                    if client_errors.count('CONSUMER_DISCONNECT_REMOVAL_OBSERVED') != (1 if disconnect_after else 0):
                        raise RuntimeError('disconnect did not deliver buffer removal')
                    if client_errors.count('LOOP_EVENT_DURING_GPU_WORKER') != (0 if native_egl else completed_frames):
                        raise RuntimeError('loop responsiveness checkpoint missing')
                    if client_errors.count('REMOVED_WORKER_DRAINED_NO_RELEASE_QUEUE_ACK') != (1 if disconnect_worker else 0):
                        raise RuntimeError('removed worker was not drained without release')
                    if native_encoder and client_errors.count('NATIVE_GPU_ENCODE_RESULT result=0 complete=1') != completed_frames:
                        raise RuntimeError('native GPU encoder completion count mismatch')
                    if portal_engine:
                        expected_bridge = {
                            'PORTAL_ENGINE_FORMAT_NEGOTIATED': 1,
                            'PORTAL_ENGINE_LEASE_ACQUIRED': completed_frames,
                            'PORTAL_ENGINE_LEASE_FINISHED': 0 if negative or disconnect_worker else completed_frames,
                            'PORTAL_ENGINE_LEASE_ABANDONED': 1 if negative or disconnect_worker else 0,
                        }
                        if any(client_errors.count(marker) != count for marker, count in expected_bridge.items()):
                            raise RuntimeError('Portal engine bridge lifecycle mismatch')
                    indices = re.findall(r'^ENCODED_FRAME index=(\d+) bytes=\d+$', client_errors, re.MULTILINE)
                    if indices != [str(i) for i in range(completed_frames)]:
                        raise RuntimeError('encoded frame sequence mismatch')
                    if (negative or disconnect_worker) and 'synthetic pnt release begin' in client_errors:
                        raise RuntimeError('negative path attempted release')
                    if negative and any(client_errors.count(marker) != 1 for marker in (
                            'INJECTED_MISMATCH_AFTER_ENCODE',
                            'NEGATIVE_PRODUCER_POINT_1_NO_SENTINEL',
                            'NEGATIVE_CONSUMER_NO_COMPLETE_RELEASE_QUEUE_ACK')):
                        raise RuntimeError('negative mismatch handshake not verified')
                    bitstream = root / 'synthetic.h264'
                    dimensions = spawn(children, ['ffprobe', '-v', 'error', '-select_streams', 'v:0',
                                       '-show_entries', 'stream=width,height', '-of', 'csv=s=x:p=0', str(bitstream)],
                                       env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    out, err = dimensions.communicate(timeout=20)
                    if dimensions.returncode or out != b'256x256\n':
                        raise RuntimeError('ffprobe dimension verification failed: ' + err.decode(errors='replace'))
                    decoder = spawn(children, ['ffmpeg', '-nostdin', '-v', 'error', '-c:v', 'h264', '-f', 'h264',
                                    '-i', str(bitstream), '-fps_mode', 'passthrough', '-f', 'rawvideo', '-pix_fmt', 'rgb24', 'pipe:1'],
                                    env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    pixels, err = decoder.communicate(timeout=20)
                    if decoder.returncode or len(pixels) != completed_frames * 256 * 256 * 3:
                        raise RuntimeError('ffmpeg software decode failed or wrong size: ' + err.decode(errors='replace'))
                    expected = ((255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255))
                    for frame in range(completed_frames):
                        for q in range(4):
                            color = expected[(q + frame) % 4]
                            for dy in range(4, 28, 2):
                                for dx in range(4, 28, 2):
                                    x, y = (q % 2) * 128 + dx, (q // 2) * 128 + dy
                                    start = (frame * 256 * 256 + y * 256 + x) * 3
                                    pixel = pixels[start:start + 3]
                                    if any(abs(a - b) > 35 for a, b in zip(pixel, color)):
                                        raise RuntimeError(f'decoded synthetic color mismatch frame={frame} quadrant={q} x={x} y={y}')
                    print(f'Software decoded {completed_frames} changing 256x256 frames: 144 samples per quadrant within tolerance 35')
            finally:
                signal.pthread_sigmask(signal.SIG_BLOCK, signals)
                for s in signals:
                    signal.signal(s, signal.SIG_IGN)
                errors = []
                if daemon is not None and daemon.poll() is not None:
                    errors.append(RuntimeError(f'daemon exited before cleanup: {daemon.returncode}'))
                for child in reversed(children):
                    if child is daemon:
                        term_sent = False
                        killed = False
                        try:
                            if child.poll() is not None:
                                errors.append(RuntimeError(f'daemon exited before SIGTERM: {child.returncode}'))
                            else:
                                os.kill(child.pid, signal.SIGTERM)
                                term_sent = True
                        except Exception as exc:
                            errors.append(exc)
                        try:
                            child.wait(timeout=2)
                        except subprocess.TimeoutExpired:
                            killed = True
                            errors.append(RuntimeError('daemon required SIGKILL'))
                            try:
                                child.kill()
                            except Exception as exc:
                                errors.append(exc)
                            try:
                                child.wait(timeout=2)
                            except Exception as exc:
                                errors.append(exc)
                                temporary.preserve = True
                        except Exception as exc:
                            errors.append(exc)
                            temporary.preserve = True
                        print(f'private daemon cleanup: SIGTERM sent={term_sent}, SIGKILL required={killed}, returncode={child.returncode}', file=sys.stderr)
                        if not term_sent or killed or child.returncode not in (0, -signal.SIGTERM):
                            errors.append(RuntimeError(f'unacceptable daemon termination: {child.returncode}'))
                        continue
                    try:
                        if child.poll() is None:
                            child.terminate()
                    except Exception as exc:
                        errors.append(exc)
                    try:
                        child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        try:
                            child.kill()
                        except Exception as exc:
                            errors.append(exc)
                        try:
                            child.wait(timeout=2)
                        except Exception as exc:
                            errors.append(exc)
                            temporary.preserve = True
                    except Exception as exc:
                        errors.append(exc)
                        temporary.preserve = True
                if client_log is not None:
                    try:
                        if any(child.poll() is None for child in children):
                            raise RuntimeError('direct child not reaped; client log unavailable')
                        client_errors, truncated = read_client_log(temporary, client_log)
                        print('--- synthetic client log ---', file=sys.stderr)
                        print(client_errors, file=sys.stderr, end='' if client_errors.endswith('\n') else '\n')
                        print('--- end synthetic client log ---', file=sys.stderr)
                        if truncated:
                            errors.append(RuntimeError('synthetic client log truncated'))
                    except Exception as exc:
                        errors.append(exc)
                    finally:
                        client_log.close()
                if daemon_log_fd >= 0:
                    try:
                        if daemon is not None and daemon.poll() is None:
                            raise RuntimeError('daemon not reaped; final log unavailable')
                        temporary.check()
                        with os.fdopen(daemon_log_fd, 'r') as final_log:
                            daemon_log_fd = -1
                            final_log.seek(0)
                            daemon_log = final_log.read()
                        if any(line.startswith('[E]') or line.startswith('[E ') for line in daemon_log.splitlines()):
                            errors.append(RuntimeError('private daemon reported error: ' + daemon_log.strip()))
                    finally:
                        if daemon_log_fd >= 0:
                            os.close(daemon_log_fd)
                if errors:
                    raise RuntimeError('child cleanup or final daemon log failed: ' + repr(errors))
    finally:
        signal.pthread_sigmask(signal.SIG_BLOCK, signals)
        try:
            while signal.sigtimedwait(signals, 0) is not None:
                pass
            for s, handler in handlers.items():
                signal.signal(s, handler)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, mask)
    if disconnect_worker:
        print('PASS WORKER_REMOVAL: real stream removal during worker checkpoint; owned descriptor retained through completion; no release/queue/ACK; local test bitstream decoded, not delivered.')
    elif disconnect_after:
        print(f'PASS DISCONNECT: {completed_frames} frames decoded and returned; real buffer removal observed; no later frame submitted or encoded. Idle encoder cleanup completed.')
    elif negative:
        print('PASS NEGATIVE_MISMATCH: failure result injected after synthetic encode (no comparison failed); no consumer complete/release/queue/ACK, producer point 1 and no sentinel.')
    else:
        print(f'PASS: {frames} synthetic frames, persistent NVENC, software-decoded changing pixels, native fences, original buffer returns and final sentinel.')
    if portal_engine:
        print('PASS PORTAL_ENGINE_BRIDGE: real Portal format/process/lease APIs connected to private PipeWire and NVIDIA worker; no real Portal session opened.')
    if flagless:
        print('PASS FLAGLESS: explicitly selected release-point contract; no release flag synthesized.')
    if native_egl:
        print('PASS NATIVE_EGL: CUDA import, layout validation, copy and unregister handled by native encoder.')
    if native_encoder:
        print('PASS NATIVE_GPU_ENCODER: imported DMA-BUF frames encoded by native nvenc_encode_gpu.')
    print('Serialized synthetic frames only; worker-checkpoint removal does not prove hardware-pending cancellation or the real compositor contract. A daemon exit of 0 racing SIGTERM cannot be distinguished from graceful termination. Synthetic exclusive signalers assumed; not Portal safety or proof that the fence wait was necessary. GL cleanup does not prove driver correctness or real revocation. Only direct children managed; exit/timeout does not isolate GPU access.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', default='/dev/dri/renderD128')
    parser.add_argument('--negative-mismatch', action='store_true', help='inject a failure result after synthetic encode (no comparison fails); successful test exits 0 and prints PASS NEGATIVE_MISMATCH')
    parser.add_argument('--frames', type=int, default=1, help='serialized frames reusing one DMA-BUF and encoder (1..32)')
    parser.add_argument('--disconnect-after', type=int, default=0, help='disconnect the consumer after N completed frames; verify no subsequent reuse')
    parser.add_argument('--disconnect-worker', action='store_true', help='disconnect at worker checkpoint after copy returns; requires one frame, not proof hardware is still busy')
    parser.add_argument('--portal-engine', action='store_true', help='use actual Portal capture/lease callbacks over the private PipeWire daemon')
    parser.add_argument('--native-encoder', action='store_true', help='use the native NVENC GPU-copy API and constructor')
    parser.add_argument('--native-egl', action='store_true', help='use native CUDA EGL import (implies native encoder; no worker checkpoint injection)')
    parser.add_argument('--flagless', action='store_true', help='explicit release-point-only producer contract (requires --portal-engine)')
    args = parser.parse_args()
    if args.flagless and not args.portal_engine:
        parser.error('--flagless requires --portal-engine')
    if args.native_egl:
        args.native_encoder = True
        if args.disconnect_worker:
            parser.error('--native-egl does not provide a worker removal checkpoint')
    if not 1 <= args.frames <= 32 or (args.negative_mismatch and args.frames != 1):
        parser.error('--frames must be 1..32; --negative-mismatch requires --frames 1')
    if args.disconnect_after and (args.negative_mismatch or not 1 <= args.disconnect_after < args.frames):
        parser.error('--disconnect-after requires 1 <= N < frames and cannot combine with --negative-mismatch')
    if args.disconnect_worker and (args.frames != 1 or args.negative_mismatch or args.disconnect_after):
        parser.error('--disconnect-worker requires one frame and no other failure mode')
    try:
        main(args.device, args.negative_mismatch, args.frames, args.disconnect_after, args.disconnect_worker, args.portal_engine, args.native_encoder, args.native_egl, args.flagless)
    except (OSError, RuntimeError, subprocess.SubprocessError, TimeoutError) as exc:
        print(f'FAIL: {exc}', file=sys.stderr)
        sys.exit(1)
