#!/usr/bin/env python3
"""Exercise a synthetic stream on a disposable, private PipeWire server."""
import os
import pathlib
import random
import re
import signal
import struct
import subprocess
import sys
import tempfile
import time
import uuid


# No device discovery, session manager, or user configuration is loaded.
CONFIG = '''
context.properties = { core.daemon = true core.name = pipewire-0 }
context.spa-libs = { support.* = support/libspa-support audio.convert.* = audioconvert/libspa-audioconvert }
context.modules = [
 { name = libpipewire-module-protocol-native }
 { name = libpipewire-module-access }
 { name = libpipewire-module-client-node }
 { name = libpipewire-module-adapter }
 { name = libpipewire-module-link-factory }
 { name = libpipewire-module-spa-node-factory }
 { name = libpipewire-module-metadata }
]
context.objects = [
 { factory = spa-node-factory args = { factory.name = support.node.driver node.name = Dummy-Driver priority.driver = 20000 } }
]
'''


def spawn(children, args, **kwargs):
    # Register before either signal can unwind; restore the child's mask before exec.
    # This program is single-threaded, so preexec_fn is safe here.
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
    try:
        child = subprocess.Popen(
            args, preexec_fn=lambda: signal.pthread_sigmask(
                signal.SIG_SETMASK, previous - {signal.SIGINT, signal.SIGTERM}), **kwargs)
        children.append(child)
        return child
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def command(children, env, *args):
    child = spawn(children, args, env=env, text=True,
                  stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    stdout, stderr = child.communicate(timeout=3)
    if child.returncode:
        raise subprocess.CalledProcessError(child.returncode, args, stdout, stderr)
    return stdout


def port_id(children, env, direction, name):
    matches = []
    for line in command(children, env, 'pw-link', '-I', direction).splitlines():
        match = re.fullmatch(r'\s*(\d+)\s+(.+)', line)
        if match and match.group(2) == name:
            matches.append(match.group(1))
    if len(matches) > 1:
        raise RuntimeError('ambiguous test port: ' + name)
    return matches[0] if matches else None


def terminate(signum, frame):
    # Ignore either signal during exception unwinding and cleanup.
    signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    raise SystemExit(128 + signum)


def main():
    signal.signal(signal.SIGINT, terminate)
    signal.signal(signal.SIGTERM, terminate)
    name = 'uu-pw-smoke-' + uuid.uuid4().hex
    producer_name, consumer_name = name + '-producer', name + '-consumer'
    children = []
    with tempfile.TemporaryDirectory(prefix='uu-pw-smoke-') as directory:
        root = pathlib.Path(directory)
        config = root / 'private.conf'
        config.write_text(CONFIG)
        source, sink = root / 'source.raw', root / 'sink.raw'
        rng = random.Random(42)
        samples = [rng.randrange(-12000, 12001) for _ in range(48000 * 10)]
        synthetic = struct.pack('<%dh' % len(samples), *samples)
        source.write_bytes(synthetic)
        # Do not inherit remote, config, module, or runtime overrides. Every client
        # uses the absolute private socket name, never the default user socket.
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(('PIPEWIRE_', 'SPA_', 'PW_'))
               and k not in ('XDG_RUNTIME_DIR', 'XDG_CONFIG_HOME', 'XDG_CONFIG_DIRS')}
        env.update(PIPEWIRE_RUNTIME_DIR=directory, XDG_RUNTIME_DIR=directory,
                   XDG_CONFIG_HOME=directory, XDG_CONFIG_DIRS=directory,
                   PIPEWIRE_REMOTE=str(root / 'pipewire-0'))
        common = ['-a', '--target', '0', '--rate', '48000', '--channels', '1',
                  '--format', 's16']
        try:
            daemon = spawn(children, ['pipewire', '-c', str(config)], env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            streams = []
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if daemon.poll() is not None:
                    raise RuntimeError('private PipeWire daemon failed to start')
                try:
                    if (root / 'pipewire-0').is_socket():
                        command(children, env, 'pw-link', '-o')
                        break
                except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
                    pass
                time.sleep(0.05)
            else:
                raise TimeoutError('private PipeWire socket not ready')
            for mode, node, filename, extra in [
                ('-p', producer_name, source, []),
                ('-r', consumer_name, sink, ['-n', '4800']),
            ]:
                props = ('{ node.name = ' + node + ' node.autoconnect = false '
                         'adapter.auto-port-config = { mode = dsp monitor = false position = preserve } }')
                streams.append(spawn(
                    children, ['pw-cat', mode, *common, '-P', props, *extra, str(filename)],
                    env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            deadline = time.monotonic() + 5
            output = input_ = None
            while time.monotonic() < deadline:
                if daemon.poll() is not None or any(child.poll() is not None for child in streams):
                    raise RuntimeError('private daemon or pw-cat exited before ports appeared')
                output = port_id(children, env, '-o', producer_name + ':output_MONO')
                input_ = port_id(children, env, '-i', consumer_name + ':input_MONO')
                if output and input_:
                    break
                time.sleep(0.05)
            if not output or not input_ or output == input_:
                raise TimeoutError('distinct private test ports did not appear')
            command(children, env, 'pw-link', output, input_)
            try:
                streams[1].wait(timeout=5)
            except subprocess.TimeoutExpired as exc:
                raise TimeoutError('consumer did not finish') from exc
            if streams[1].returncode not in (0, 1):
                raise RuntimeError('consumer failed')
            data = sink.read_bytes()
            if len(data) != 9600:
                raise RuntimeError('incomplete recording')
            # Find a substantial exactly aligned run, not just a constant marker.
            # Startup may contain silence before the link becomes active.
            if not any(synthetic.find(data[i:i + 4096]) % 2 == 0
                       for i in range(0, len(data) - 4096 + 1, 2)):
                raise RuntimeError('missing 2048 aligned synthetic samples')
            print('PASS: private PipeWire link transferred 2048 aligned synthetic samples')
        finally:
            signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            errors = []
            for child in reversed(children):
                try:
                    if child.poll() is None:
                        child.terminate()
                except Exception as exc:
                    errors.append(exc)
            for child in reversed(children):
                try:
                    try:
                        child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=2)
                except Exception as exc:
                    errors.append(exc)
            if errors:
                print('child cleanup errors: ' + repr(errors), file=sys.stderr)
                raise RuntimeError('failed to clean up all child processes')


if __name__ == '__main__':
    main()
