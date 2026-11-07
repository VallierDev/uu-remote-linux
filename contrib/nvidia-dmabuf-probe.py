#!/usr/bin/env python3
"""Read-only prerequisites probe; does not capture, import, encode, or prove zero-copy."""
import ctypes
import glob
import os
import sys


def load(name):
    try:
        return ctypes.CDLL(name)
    except OSError:
        return None


def pci_key(value):
    """Normalize CUDA's 0000:bb:dd.f and sysfs PCI identifiers."""
    try:
        domain, bus, slot_func = value.lower().split(':')
        slot, func = slot_func.split('.')
        return (int(domain, 16), int(bus, 16), int(slot, 16), int(func, 16))
    except (ValueError, AttributeError):
        return None


def render_nodes(pci, base='/sys/class/drm'):
    result = []
    for node in sorted(glob.glob(os.path.join(base, 'renderD[0-9]*'))):
        device = os.path.basename(os.path.realpath(os.path.join(node, 'device')))
        if pci_key(device) == pci and pci is not None:
            result.append('/dev/dri/' + os.path.basename(node))
    return result


def main():
    cuda = load('libcuda.so.1')
    nvenc = load('libnvidia-encode.so.1')
    print('CUDA driver:', 'available' if cuda else 'unavailable')
    print('NVENC library:', 'available' if nvenc else 'unavailable')
    if not cuda:
        print('INCONCLUSIVE: DMA-BUF import and encoding not tested')
        return 1
    for symbol in ('cuInit', 'cuDeviceGetCount', 'cuDeviceGet', 'cuDeviceGetPCIBusId'):
        if not hasattr(cuda, symbol):
            print('INCONCLUSIVE: missing CUDA symbol', symbol)
            return 1
    print('CUDA external-memory entry point:',
          'present' if hasattr(cuda, 'cuImportExternalMemory') else 'absent')
    rc = cuda.cuInit(0)
    if rc:
        print('INCONCLUSIVE: cuInit failed:', rc)
        return 1
    count = ctypes.c_int()
    rc = cuda.cuDeviceGetCount(ctypes.byref(count))
    if rc:
        print('INCONCLUSIVE: cuDeviceGetCount failed:', rc)
        return 1
    if count.value <= 0:
        print('INCONCLUSIVE: no CUDA GPUs found')
        return 1
    failed = False
    for ordinal in range(count.value):
        device = ctypes.c_int()
        bus = ctypes.create_string_buffer(32)
        if cuda.cuDeviceGet(ctypes.byref(device), ordinal) or cuda.cuDeviceGetPCIBusId(bus, len(bus), device):
            print('CUDA device', ordinal, ': PCI identity unavailable')
            failed = True
            continue
        pci = bus.value.decode('ascii', errors='replace')
        key = pci_key(pci)
        if key is None:
            print('CUDA device', ordinal, ': invalid PCI identity', pci)
            failed = True
            continue
        nodes = render_nodes(key)
        print('CUDA device', ordinal, ': PCI', pci, 'render nodes:', ', '.join(nodes) or 'none found')
    if nvenc:
        try:
            query = nvenc.NvEncodeAPIGetMaxSupportedVersion
            version = ctypes.c_uint32()
            rc = query(ctypes.byref(version))
            if rc == 0:
                print('NVENC maximum API: %d.%d' % (version.value >> 4, version.value & 15))
            else:
                print('NVENC API query failed:', rc)
                failed = True
        except AttributeError:
            print('NVENC API query unavailable')
            failed = True
    else:
        print('NVENC API query unavailable: library missing')
        failed = True
    print('INCONCLUSIVE: no Portal buffer imported or encoded; GPU match, synchronization,')
    print('format/modifier compatibility and copy behavior remain unverified.')
    print('Exit 0 means prerequisite queries completed only; DMA-BUF was not verified.')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
