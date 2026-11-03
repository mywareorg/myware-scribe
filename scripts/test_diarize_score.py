#!/usr/bin/env python3
"""Tests for the diarization evaluation harness.

    python3 scripts/test_diarize_score.py
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("diarize_score", HERE / "diarize-score.py")
assert SPEC and SPEC.loader
harness = importlib.util.module_from_spec(SPEC)
sys.modules["diarize_score"] = harness
SPEC.loader.exec_module(harness)


def write(directory: Path, name: str, body: str) -> Path:
    path = directory / name
    path.write_text(body, encoding="utf-8")
    return path


RTTM = """\
SPEAKER f 1 0.00 5.00 <NA> <NA> spk0 <NA> <NA>
SPEAKER f 1 5.20 3.00 <NA> <NA> spk0 <NA> <NA>
SPEAKER f 1 8.40 0.60 <NA> <NA> spk1 <NA> <NA>
SPEAKER f 1 9.20 4.00 <NA> <NA> spk0 <NA> <NA>
"""

MATCHING_TSV = """\
0.05\t5.00\tA
5.25\t8.25\tA
8.45\t8.95\tB
9.30\t13.10\tA
"""


class HarnessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self) -> None:
        self.directory.cleanup()

    def score(self, rttm: str, tsv: str):
        ref = harness.read_rttm(write(self.root, "ref.rttm", rttm))
        hyp = harness.read_tsv(write(self.root, "hyp.tsv", tsv))
        return harness.score(ref, hyp, collar=0.25, block=300.0)

    def test_parsing_skips_bad_rows(self) -> None:
        tsv = write(
            self.root,
            "turns.tsv",
            "0.0\t1.0\tA\n# comment\n1.5\t1.2\tB\n2.0\t3.0\t\n",
        )
        turns = harness.read_tsv(tsv)
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0].speaker, "A")

        rttm = write(
            self.root,
            "ref.rttm",
            "SPEAKER f 1 0.0 1.0 <NA> <NA> spk0 <NA> <NA>\n"
            "not-a-speaker-line\n",
        )
        self.assertEqual(len(harness.read_rttm(rttm)), 1)

    def test_exact_hypothesis_scores_zero(self) -> None:
        result = self.score(RTTM, MATCHING_TSV)
        self.assertLess(result.der, 0.01)
        self.assertEqual(result.short_turns, 1)
        self.assertEqual(result.short_turns_found, 1)

    def test_speaker_names_do_not_matter(self) -> None:
        swapped = MATCHING_TSV.replace("\tA", "\tX").replace("\tB", "\tY")
        result = self.score(RTTM, swapped)
        self.assertLess(result.der, 0.01)

    def test_missed_and_false_alarm_are_separated(self) -> None:
        # Drop the short turn and add a turn after the reference speech.
        hyp = "0.05\t5.00\tA\n5.25\t8.25\tA\n9.30\t13.10\tA\n13.40\t14.00\tA\n"
        result = self.score(RTTM, hyp)
        self.assertGreater(result.missed_seconds, 0.0)
        self.assertGreater(result.false_alarm_seconds, 0.0)
        self.assertEqual(result.confusion_seconds, 0.0)

    def test_collar_absorbs_small_boundary_jitter(self) -> None:
        # Shift every boundary by 150 ms; inside the 250 ms collar.
        jittered = """\
0.20\t5.15\tA
5.35\t8.35\tA
8.55\t9.05\tB
9.35\t13.15\tA
"""
        result = self.score(RTTM, jittered)
        self.assertLess(result.der, 0.05)

    def test_confusion_is_counted_for_the_wrong_speaker(self) -> None:
        hyp = "0.05\t5.00\tA\n5.25\t8.25\tB\n8.45\t8.95\tB\n9.30\t13.10\tA\n"
        result = self.score(RTTM, hyp)
        self.assertGreater(result.confusion_seconds, 1.0)

    def test_draft_round_trips_through_rttm(self) -> None:
        tsv = write(self.root, "turns.tsv", MATCHING_TSV)
        turns = harness.read_tsv(tsv)
        body = "\n".join(harness.rttm_line(turn, "f") for turn in turns)
        rttm = write(self.root, "draft.rttm", body + "\n")
        parsed = harness.read_rttm(rttm)
        self.assertEqual(len(parsed), len(turns))
        for expected, actual in zip(turns, parsed):
            self.assertAlmostEqual(expected.start, actual.start, places=3)
            self.assertAlmostEqual(expected.end, actual.end, places=3)
            self.assertEqual(expected.speaker, actual.speaker)

    def test_transcript_lines_become_capped_turns(self) -> None:
        transcript = write(
            self.root,
            "corrected.txt",
            "[0:00] SPEAKER 1: one two three four\n"
            "[0:02] SPEAKER 2: yes\n"
            "[0:40] SPEAKER 1: a much later line\n",
        )
        turns = harness.read_transcript(transcript, seconds_per_word=0.33, max_turn=15.0)
        self.assertEqual(len(turns), 3)
        self.assertEqual([turn.speaker for turn in turns], ["SPEAKER 1", "SPEAKER 2", "SPEAKER 1"])
        # Four words estimate to 1.62 s, so the turn ends before the next line.
        self.assertAlmostEqual(turns[0].end, 1.62, places=2)
        self.assertLessEqual(turns[0].end, turns[1].start)
        # The last turn falls back to the estimate instead of running forever.
        self.assertLessEqual(turns[2].end - turns[2].start, 15.0)
        self.assertGreater(turns[2].end - turns[2].start, 0.5)

    def test_transcript_without_speakers_uses_unknown(self) -> None:
        transcript = write(self.root, "plain.txt", "[00:05] just text here\n")
        turns = harness.read_transcript(transcript, 0.33, 15.0)
        self.assertEqual(turns[0].speaker, "unknown")
        self.assertAlmostEqual(turns[0].start, 5.0, places=3)

    def test_speaker_names_are_one_rttm_token(self) -> None:
        turn = harness.Turn(0.0, 1.0, "SPEAKER 1")
        self.assertIn(" SPEAKER_1 ", harness.rttm_line(turn, "f"))
        self.assertEqual(harness.rttm_speaker("  "), "unknown")

    def test_overlap_is_encoded_by_a_shared_timestamp(self) -> None:
        transcript = write(
            self.root,
            "overlap.txt",
            "[0:10] SPEAKER 1: one two three\n"
            "[0:10] SPEAKER 2: four five six\n"
            "[0:20] SPEAKER 1: after\n",
        )
        turns = harness.read_transcript(transcript, seconds_per_word=0.33, max_turn=15.0)
        self.assertEqual(len(turns), 3)
        # Both lines keep their own estimate instead of collapsing to 0.2 s.
        self.assertGreater(turns[0].end - turns[0].start, 1.0)
        self.assertGreater(turns[1].end - turns[1].start, 1.0)

    def test_overlapping_reference_accepts_either_speaker(self) -> None:
        rttm = (
            "SPEAKER f 1 0.0 5.0 <NA> <NA> spk0 <NA> <NA>\n"
            "SPEAKER f 1 2.0 2.0 <NA> <NA> spk1 <NA> <NA>\n"
        )
        hyp = "0.0\t5.0\tA\n"  # only the dominant speaker is emitted
        ref = harness.read_rttm(write(self.root, "ref.rttm", rttm))
        hypothesis = harness.read_tsv(write(self.root, "hyp.tsv", hyp))
        result = harness.score(ref, hypothesis, collar=0.25, block=300.0)
        self.assertGreaterEqual(result.overlap_seconds, 1.5)
        self.assertLess(result.der, 0.05)

        excluded = harness.score(
            ref, hypothesis, collar=0.25, block=300.0, ignore_overlap=True
        )
        self.assertLess(excluded.reference_seconds, result.reference_seconds)

    def test_restrict_clips_turns_to_a_window(self) -> None:
        turns = [harness.Turn(0.0, 10.0, "A"), harness.Turn(20.0, 30.0, "B")]
        clipped = harness.restrict(turns, 5.0, 25.0)
        self.assertEqual(len(clipped), 2)
        self.assertAlmostEqual(clipped[0].start, 5.0)
        self.assertAlmostEqual(clipped[1].end, 25.0)
        # Nothing overlapping the window is dropped silently.
        self.assertEqual(harness.restrict(turns, 40.0, 50.0), [])

    def test_frame_dominant_prefers_the_longest_turn(self) -> None:
        turns = [
            harness.Turn(0.0, 4.0, "long"),
            harness.Turn(1.0, 1.5, "short"),
        ]
        labels, durations = harness.frame_dominant(turns, 500)
        self.assertEqual(labels[110], "long")
        self.assertAlmostEqual(durations[110], 4.0)
        self.assertEqual(labels[10], "long")
        self.assertIsNone(labels[450])

    def test_confusion_is_bucketed_by_reference_turn_length(self) -> None:
        rttm = (
            "SPEAKER f 1 0.0 60.0 <NA> <NA> spk0 <NA> <NA>\n"
            "SPEAKER f 1 60.0 1.0 <NA> <NA> spk1 <NA> <NA>\n"
            "SPEAKER f 1 61.0 0.9 <NA> <NA> spk0 <NA> <NA>\n"
            "SPEAKER f 1 61.9 3.0 <NA> <NA> spk1 <NA> <NA>\n"
            "SPEAKER f 1 64.9 2.0 <NA> <NA> spk0 <NA> <NA>\n"
        )
        hyp = (
            "0.0\t60.0\tA\n60.0\t61.0\tB\n61.0\t61.9\tB\n"
            "61.9\t64.9\tA\n64.9\t66.9\tB\n"
        )
        result = self.score(rttm, hyp)
        buckets = result.bucket_confusion
        # Each collared turn keeps some measurable confusion in its own bucket.
        self.assertGreater(buckets.get("<1s", 0.0), 0.0)
        self.assertGreater(buckets.get("1-3s", 0.0), 1.0)
        self.assertGreater(buckets.get(">3s", 0.0), 2.0)
        self.assertAlmostEqual(
            sum(buckets.values()), result.confusion_seconds, places=1
        )

    def test_duration_bucket_boundaries(self) -> None:
        self.assertEqual(harness.duration_bucket(0.9), "<1s")
        self.assertEqual(harness.duration_bucket(1.0), "1-3s")
        self.assertEqual(harness.duration_bucket(2.9), "1-3s")
        self.assertEqual(harness.duration_bucket(3.0), ">3s")

    def test_block_matrix_separates_missed_from_confused(self) -> None:
        rttm = (
            "SPEAKER f 1 0.0 5.0 <NA> <NA> spk0 <NA> <NA>\n"
            "SPEAKER f 1 5.0 5.0 <NA> <NA> spk1 <NA> <NA>\n"
        )
        # Speaker A is right, speaker B is dropped entirely.
        hyp = "0.0\t5.0\tA\n"
        result = self.score(rttm, hyp)
        matrix_ref, matrix_hyp = next(
            (ref_name, hyp_name)
            for (ref_name, hyp_name), _ in result.block_confusion[0].items()
            if ref_name != "spk1" and hyp_name is not None
        )
        matrix = result.block_confusion.get(0, {})
        self.assertGreater(matrix.get(("spk1", None), 0.0), 4.4)
        # The matrix carries the diagonal too: A really was correct for 5 s.
        self.assertGreater(matrix.get((matrix_ref, matrix_hyp), 0.0), 4.4)

    def test_ref_range_and_offset_score_a_clip(self) -> None:
        rttm = write(
            self.root,
            "ref.rttm",
            "SPEAKER f 1 0.0 100.0 <NA> <NA> spk0 <NA> <NA>\n",
        )
        # A clip cut from 40 s in, so its own timeline starts at zero.
        tsv = write(self.root, "clip.tsv", "0.0\t20.0\tA\n")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = harness.main(
                [
                    "score",
                    "--ref",
                    str(rttm),
                    "--hyp",
                    str(tsv),
                    "--ref-range",
                    "40",
                    "60",
                    "--hyp-offset",
                    "40",
                ]
            )
        self.assertEqual(code, 0)
        self.assertIn("DER @0.25s      :    0.00%", out.getvalue())
        # The window is clipped on both sides, so nothing lands as false alarm.
        self.assertIn("false alarm      :     0.00s", out.getvalue())

    def test_speakers_expect_fails_on_a_phantom(self) -> None:
        tsv = write(self.root, "turns.tsv", MATCHING_TSV)
        self.assertEqual(harness.main(["speakers", str(tsv), "--expect", "2"]), 0)
        self.assertEqual(harness.main(["speakers", str(tsv), "--expect", "1"]), 1)

    def test_compare_reports_added_removed_and_relabelled(self) -> None:
        golden = [harness.Turn(0.0, 1.0, "A"), harness.Turn(1.0, 2.0, "A")]
        same = [harness.Turn(0.01, 1.0, "A"), harness.Turn(1.0, 2.0, "A")]
        self.assertEqual(harness.diff_turns(golden, same, 0.05), [])

        relabelled = [harness.Turn(0.0, 1.0, "B"), harness.Turn(1.0, 2.0, "A")]
        self.assertEqual(len(harness.diff_turns(golden, relabelled, 0.05)), 1)

        removed = [harness.Turn(1.0, 2.0, "A")]
        self.assertEqual(len(harness.diff_turns(golden, removed, 0.05)), 1)

        # One golden turn disappears and one unknown turn appears.
        added = [harness.Turn(0.0, 1.0, "A"), harness.Turn(2.5, 3.0, "B")]
        self.assertEqual(len(harness.diff_turns(golden, added, 0.05)), 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
