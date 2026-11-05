"""Offline checks for excluding warmup/teardown and failed performance runs."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'sampler', Path(__file__).resolve().parents[1] / 'contrib/portal-resource-sample.py')
sampler = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sampler)


class MeasurementWindow(unittest.TestCase):
    start = 'Measurement begin: monotonic_us=10000000 warmup=30\n'
    end = 'Measurement: complete=1 monotonic_us=20000000 frames=300\n'

    def test_summary_ignores_missing_values_and_uses_nearest_rank_p95(self):
        self.assertIsNone(sampler.summary([None]))
        result = sampler.summary([None] + list(range(1, 21)))
        self.assertEqual(result['mean'], 10.5)
        self.assertEqual(result['median'], 10.5)
        self.assertEqual(result['p95'], 19)

    def test_completed_interval(self):
        self.assertEqual(sampler.measurement_window(self.start + self.end), (10, 20))

    def test_incomplete_duplicate_or_reversed_interval(self):
        for log in (self.start, self.end, self.start + self.end.replace('complete=1', 'complete=0'),
                    self.start * 2 + self.end, self.start + self.end * 2,
                    self.start + self.end.replace('20000000', '9000000')):
            with self.subTest(log=log), self.assertRaises(RuntimeError):
                sampler.measurement_window(log)

    def test_boundary_crossing_samples_excluded(self):
        rows = [{'start': a, 'end': b} for a, b in
                ((9, 10.1), (10, 11.1), (11, 12.1), (12, 13.1), (19, 20.1), (20, 21))]
        self.assertEqual(sampler.select_window(rows, (10, 20)), rows[1:4])

    def test_insufficient_or_stale_samples_rejected(self):
        rows = [{'start': 10, 'end': 11}, {'start': 11, 'end': 12}]
        for window in ((10, 20), (100, 200)):
            with self.assertRaises(RuntimeError):
                sampler.select_window(rows, window)


if __name__ == '__main__':
    unittest.main()
