#!/usr/bin/env python3
"""Cross-process synthetic EGL/CUDA and CPU-signaled timeline test, not Portal safety.

Only self-created BOs are used. Exit or timeout does not isolate GPU access.
Positive and --negative-mismatch success both exit 0; distinguish the mode by
the command line and its explicit PASS or PASS NEGATIVE_MISMATCH marker.

Trust boundary: root and same-UID processes are trusted not to replace paths;
filesystem ACLs and the mount namespace must not grant other users write access
or path replacement. CC, CFLAGS, LDFLAGS, PATH and LD_* are trusted test inputs.
Path checks are not cryptographic authentication of the daemon or an adversarial
same-UID/environment test.

Cleanup manages and reaps only direct children; tools such as CC may spawn
unmanaged descendants. No process-group cleanup or complete cleanup after
abnormal termination is guaranteed. Consumer queue success is not proof that
the producer dequeued the returned buffer.
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


def main(device, negative=False):
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
                pkg = spawn(children, ['pkg-config', '--cflags', '--libs', 'libpipewire-0.3', 'gbm', 'libdrm', 'egl', 'glesv2'],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                flags, errors = pkg.communicate(timeout=10)
                if pkg.returncode:
                    raise RuntimeError('missing build dependencies: ' + errors.strip())
                temporary.check()
                cc = spawn(children, [*shlex.split(os.environ.get('CC', 'cc')), '-std=gnu11', '-Wall', '-Wextra',
                                      '-Werror', '-O1', '-g', *shlex.split(os.environ.get('CFLAGS', '')),
                                      '-I' + str(pathlib.Path(__file__).resolve().parents[2] / 'vendor/ffnvcodec'),
                                      str(pathlib.Path(__file__).with_suffix('.c')),
                                      str(pathlib.Path(__file__).resolve().parents[2] / 'c_engine/portal_nvenc_egl_attrs.c'), '-o', str(binary),
                                      *shlex.split(flags), *shlex.split(os.environ.get('LDFLAGS', ''))])
                if cc.wait(timeout=30):
                    raise RuntimeError('compilation failed')
                env = {k: v for k, v in os.environ.items() if not k.startswith(('PIPEWIRE_', 'SPA_', 'PW_'))
                       and k not in ('XDG_RUNTIME_DIR', 'XDG_CONFIG_HOME', 'XDG_CONFIG_DIRS')}
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
                                                    producer_name, '0'], env=env, pass_fds=(left.fileno(),), stderr=client_log)
                        temporary.check(require_socket=True)
                        consumer = spawn(children, [str(binary), 'consumer', str(right.fileno()), device,
                                                    consumer_name, str(producer.pid)], env=env,
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
                        if (producer.poll() not in (None, 0) or consumer.poll() not in (None, 0)):
                            raise RuntimeError('client failed')
                        if time.monotonic() > deadline:
                            raise TimeoutError('clients timed out')
                        time.sleep(.01)
                    if producer.returncode or consumer.returncode or daemon.poll() is not None:
                        raise RuntimeError('client or daemon failed')
                    client_log.flush()
                    client_log.seek(0)
                    client_errors = client_log.read()
                    if negative and any(client_errors.count(marker) != 1 for marker in (
                            'INJECTED_MISMATCH_AFTER_VALID_SAMPLES',
                            'NEGATIVE_PRODUCER_POINT_1_NO_SENTINEL',
                            'NEGATIVE_CONSUMER_NO_COMPLETE_RELEASE_QUEUE_ACK')):
                        raise RuntimeError('negative mismatch handshake not verified')
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
                    client_log.flush()
                    client_log.seek(0)
                    client_errors = client_log.read()
                    client_log.close()
                    if client_errors:
                        print(client_errors, file=sys.stderr, end='')
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
    if negative:
        print('PASS NEGATIVE_MISMATCH: valid CUDA samples then injected failure; no consumer complete/release/queue/ACK, producer point 1 and no sentinel.')
    else:
        print('PASS: synthetic CUDA pixels, CPU-signaled syncobj and sentinel (not a native GPU fence).')
    print('Only local queue and handshake observed; no proof about pending remote events or producer dequeue. A daemon exit of 0 racing SIGTERM cannot be distinguished from graceful termination. Synthetic exclusive signalers assumed; not Portal safety. GL cleanup does not prove driver correctness or real revocation. Only direct children managed; exit/timeout does not isolate GPU access.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', default='/dev/dri/renderD128')
    parser.add_argument('--negative-mismatch', action='store_true', help='inject failure after valid CUDA samples; expected negative success exits 0 and prints PASS NEGATIVE_MISMATCH (positive success also exits 0)')
    args = parser.parse_args()
    try:
        main(args.device, args.negative_mismatch)
    except (OSError, RuntimeError, subprocess.SubprocessError, TimeoutError) as exc:
        print(f'FAIL: {exc}', file=sys.stderr)
        sys.exit(1)
