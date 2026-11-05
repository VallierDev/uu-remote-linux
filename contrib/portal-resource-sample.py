#!/usr/bin/env python3
"""Read-only resource sampling; never captures frames or changes a service.
CPU 100% means one logical core. GPU readings cover the whole selected GPU,
not just this service. Use identical workloads for comparison; this tool alone
cannot establish a before/after low-copy performance improvement.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import math
import subprocess
import time
import re
import datetime


def service_stats(service):
    output = subprocess.check_output([
        'systemctl', '--user', 'show', service, '-p', 'MainPID',
        '-p', 'CPUUsageNSec', '-p', 'MemoryCurrent', '-p', 'ActiveState',
    ], text=True)
    data = dict(line.split('=', 1) for line in output.splitlines() if '=' in line)
    if data.get('ActiveState') != 'active':
        raise RuntimeError('target service is not active')
    return {key: int(data[key]) for key in ('MainPID', 'CPUUsageNSec', 'MemoryCurrent')}


def compositor_stats():
    found = subprocess.run(['pgrep', '-x', 'gnome-shell'], capture_output=True, text=True)
    if found.returncode not in (0, 1):
        raise RuntimeError('compositor discovery failed')
    values = {}
    for pid in found.stdout.split():
        try:
            fields = Path('/proc', pid, 'stat').read_text().rsplit(')', 1)[1].split()
            values[(pid, fields[19])] = (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')
        except FileNotFoundError:
            pass
    return values


def gpu_stats(index):
    keys = ('gpu_percent', 'memory_busy_percent', 'encoder_percent', 'memory_mib', 'power_w')
    query = 'utilization.gpu,utilization.memory,utilization.encoder,memory.used,power.draw'
    values = subprocess.check_output([
        'nvidia-smi', '-i', str(index), '--query-gpu=' + query,
        '--format=csv,noheader,nounits',
    ], text=True).strip().split(',')
    if len(values) != len(keys):
        raise RuntimeError('unexpected GPU sample shape')
    result = {}
    for key, value in zip(keys, values):
        try:
            result[key] = float(value.strip())
        except ValueError:
            result[key] = None
    return result


def summary(values):
    values = [value for value in values if value is not None]
    if not values:
        return None
    return {key: round(value, 2) for key, value in {
        'mean': statistics.fmean(values), 'median': statistics.median(values),
        'p95': sorted(values)[max(0, math.ceil(len(values) * 0.95) - 1)],
        'min': min(values), 'max': max(values),
    }.items()}


def measurement_window(log):
    """Accept exactly one completed probe interval; never print log contents."""
    starts = re.findall(r'^Measurement begin: monotonic_us=(\d+) ', log, re.M)
    ends = re.findall(r'^Measurement: complete=([01]) monotonic_us=(\d+) ', log, re.M)
    if len(starts) != 1 or len(ends) != 1 or ends[0][0] != '1':
        raise RuntimeError('expected exactly one completed measurement interval')
    start, end = int(starts[0]) / 1e6, int(ends[0][1]) / 1e6
    if end <= start:
        raise RuntimeError('invalid measurement interval')
    return start, end


def select_window(samples, window):
    start, end = window
    # Include the entire CPU delta AND GPU query, not only the query midpoint.
    selected = [row for row in samples if row['start'] >= start and row['end'] <= end]
    if len(selected) < 3:
        raise RuntimeError('fewer than three samples wholly inside measurement interval')
    return selected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--service', default='uuremote-native.service')
    parser.add_argument('--seconds', type=int, default=15, choices=range(3, 121), metavar='3..120')
    parser.add_argument('--gpu', type=int, default=0)
    parser.add_argument('--label', default='current-installed-service')
    parser.add_argument('--window-log', type=Path,
                        help='probe log from this boot; keep only its completed measurement interval')
    args = parser.parse_args()
    previous = service_stats(args.service)
    initial_pid = previous['MainPID']
    if initial_pid <= 0:
        raise RuntimeError('target service has no main process')
    compositor = compositor_stats()
    started_utc = datetime.datetime.now(datetime.timezone.utc).isoformat()
    start = last = time.monotonic()
    samples = []
    for i in range(args.seconds):
        time.sleep(max(0, start + i + 1 - time.monotonic()))
        current = service_stats(args.service)
        next_compositor = compositor_stats()
        now = time.monotonic()
        if current['MainPID'] != initial_pid or current['CPUUsageNSec'] < previous['CPUUsageNSec']:
            raise RuntimeError('service restarted; sample is not comparable')
        elapsed = now - last
        cpu = 100 * (current['CPUUsageNSec'] - previous['CPUUsageNSec']) / 1e9 / elapsed
        shell_cpu = None
        if compositor and compositor.keys() == next_compositor.keys():
            shell_cpu = 100 * (sum(next_compositor.values()) - sum(compositor.values())) / elapsed
        metrics = {
            'service_cpu_percent': cpu,
            'compositor_cpu_percent': shell_cpu,
            'service_cgroup_memory_mib': current['MemoryCurrent'] / 1024**2,
            **gpu_stats(args.gpu),
        }
        samples.append({'start': last, 'end': time.monotonic(), 'metrics': metrics})
        previous, compositor, last = current, next_compositor, now
    window = measurement_window(args.window_log.read_text()) if args.window_log else None
    if window:
        samples = select_window(samples, window)
    metrics = [row['metrics'] for row in samples]
    print(json.dumps({
        'label': args.label, 'duration_seconds': round(last - start, 2),
        'started_utc': started_utc,
        'ended_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'measurement_start_monotonic': samples[0]['start'],
        'measurement_end_monotonic': samples[-1]['end'],
        'main_pid': initial_pid,
        'raw_samples': samples,
        'samples': len(samples), 'cpu_100_percent': 'one_logical_core',
        'gpu_scope': 'entire_gpu_including_other_apps',
        'service_scope': 'service_cgroup_including_children',
        'performance_comparison': False,
        'measurement_window_seconds': round(window[1] - window[0], 3) if window else None,
        'sample_query_span_seconds': round(samples[-1]['end'] - samples[0]['start'], 3),
        'gpu_sampling_note': 'whole GPU; driver utilization averaging window may cross interval boundaries',
        'metrics': {key: summary([sample[key] for sample in metrics]) for key in metrics[0]},
    }, indent=2))


if __name__ == '__main__':
    main()
