#!/usr/bin/env python3
"""Diarization evaluation harness.

Turns the CLI's `--diarize-only` TSV output into numbers, so a change to the
diarizer can be judged instead of guessed at.

Subcommands
-----------

    from-transcript <corrected.txt> <file-id> [--out ref.rttm]
        Build a reference from a transcript corrected in the app (double-click a
        line to flip speaker, right-click to edit it). Turn ends are inferred
        from the next line's start and a words-per-second estimate, so the
        result measures speaker attribution, not exact turn timing.

    draft  <turns.tsv> <file-id> [--out ref.rttm]
        Convert an existing turn map into an RTTM skeleton. Correcting a draft
        is far cheaper than labelling from scratch.

    score  --ref ref.rttm --hyp turns.tsv [--collars 0.25 1.0] [--block 300]
        Diarization error rate against a reference: missed speech, false alarm,
        speaker confusion, short-turn recall, confusion by reference turn
        length, and DER per block (optionally with a ref-vs-hyp matrix).
        Reports DER at each collar; the first drives the detailed breakdown.
        `--ref-range START END` scores only a window of the reference and
        `--hyp-offset S` shifts a clip's turns onto the reference timeline.

    speakers <turns.tsv> [--expect N]
        Count distinct speaker labels, and fail when the count is unexpected --
        the phantom-speaker check for a known single-speaker file.

    record --bin <hippo-transcribe> --out <dir> <audio>...
        Run `--diarize-only` for each file and store `dir/<stem>.tsv` as the
        baseline for that audio.

    compare <golden-dir> <run-dir> [--tolerance 0.05] [--subset]
        Diff two recorded baselines. Exits 1 when a turn was added, removed, or
        changed speaker -- intended for the regression gate. `--subset` ignores
        baseline files the run did not produce, for checking one file at a time.

RTTM lines are the standard `<type> <file> <channel> <start> <duration> ...`
form with the speaker in field 8. Only `SPEAKER` records are read.

Everything is standard library; no scipy/numpy required.
"""

from __future__ import annotations

import argparse
import itertools
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

FRAME = 0.01  # 10 ms resolution, the usual granularity for DER


TRANSCRIPT_LINE = re.compile(
    r"^\[(\d+):(\d+)(?::(\d+))?\]\s*(?:([^:]{1,32}):\s*)?(.*)$"
)


@dataclass(frozen=True)
class Turn:
    start: float
    end: float
    speaker: str

    @property
    def duration(self) -> float:
        return max(0.0, self.end - self.start)


def read_tsv(path: Path) -> list[Turn]:
    turns: list[Turn] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        start, end, speaker = float(parts[0]), float(parts[1]), parts[2].strip()
        if end > start:
            turns.append(Turn(start, end, speaker))
    return turns


def read_rttm(path: Path) -> list[Turn]:
    turns: list[Turn] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        parts = line.split()
        if not parts or parts[0] != "SPEAKER" or len(parts) < 8:
            continue
        start = float(parts[3])
        duration = float(parts[4])
        speaker = parts[7]
        if duration > 0:
            turns.append(Turn(start, start + duration, speaker))
    return turns



def read_transcript(
    path: Path, seconds_per_word: float, max_turn: float
) -> list[Turn]:
    """Turns from a `[h:mm:ss] SPEAKER: text` transcript.

    A transcript line only carries a start time. The end is the smaller of the
    next line's start and an estimate from the word count, so a turn does not
    swallow the silence before the next utterance.
    """
    rows: list[tuple[float, str, str]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        match = TRANSCRIPT_LINE.match(line.strip())
        if not match:
            continue
        first, second, third, speaker, text = match.groups()
        if third is None:
            start = int(first) * 60 + int(second)
        else:
            start = int(first) * 3600 + int(second) * 60 + int(third)
        rows.append((float(start), (speaker or "unknown").strip(), text))

    turns: list[Turn] = []
    for index, (start, speaker, text) in enumerate(rows):
        words = len(text.split())
        estimate = min(max_turn, max(0.6, words * seconds_per_word + 0.3))
        # Cap by the next *strictly later* start. Lines sharing a start encode
        # overlapping speech, and must keep their own duration.
        limit = next(
            (row[0] for row in rows[index + 1 :] if row[0] > start),
            start + estimate,
        )
        end = min(start + estimate, limit)
        if end <= start:
            end = start + 0.2
        turns.append(Turn(start, end, speaker))
    return turns


def rttm_speaker(name: str) -> str:
    """RTTM fields are whitespace-separated; speaker names must be one token."""
    collapsed = re.sub(r"\s+", "_", name.strip())
    return collapsed or "unknown"


def rttm_line(turn: Turn, file_id: str) -> str:
    return (
        f"SPEAKER {file_id} 1 {turn.start:.3f} {turn.duration:.3f} "
        f"<NA> <NA> {rttm_speaker(turn.speaker)} <NA> <NA>"
    )


def frame_sets(turns: list[Turn], frames: int) -> list[set[str]]:
    """Every speaker active in each 10 ms frame.

    More than one entry means overlapping speech in the reference.
    """
    labels: list[set[str]] = [set() for _ in range(frames)]
    for turn in turns:
        first = int(turn.start / FRAME)
        last = min(frames, int(turn.end / FRAME))
        for index in range(max(0, first), last):
            labels[index].add(turn.speaker)
    return labels


def frame_labels(turns: list[Turn], frames: int) -> list[str | None]:
    """Single label per 10 ms frame; a later turn wins where turns overlap."""
    labels: list[str | None] = [None] * frames
    for turn in turns:
        first = int(turn.start / FRAME)
        last = min(frames, int(turn.end / FRAME))
        for index in range(max(0, first), last):
            labels[index] = turn.speaker
    return labels


def frame_dominant(turns: list[Turn], frames: int) -> tuple[list[str | None], list[float]]:
    """Longest reference turn covering each 10 ms frame, and its duration.

    Overlapping speech still has a dominant speaker; using it for the mapping
    and for the confusion buckets keeps both stable instead of depending on
    set iteration order.
    """
    labels: list[str | None] = [None] * frames
    durations = [0.0] * frames
    for turn in turns:
        first = int(turn.start / FRAME)
        last = min(frames, int(turn.end / FRAME))
        for index in range(max(0, first), last):
            if turn.duration > durations[index]:
                durations[index] = turn.duration
                labels[index] = turn.speaker
    return labels, durations


def duration_bucket(seconds: float) -> str:
    if seconds < 1.0:
        return "<1s"
    if seconds < 3.0:
        return "1-3s"
    return ">3s"


def collar_mask(turns: list[Turn], frames: int, collar: float) -> list[bool]:
    """Frames excluded from scoring around every reference boundary."""
    mask = [False] * frames
    span = int(collar / FRAME)
    for turn in turns:
        for boundary in (turn.start, turn.end):
            centre = int(boundary / FRAME)
            for index in range(max(0, centre - span), min(frames, centre + span)):
                mask[index] = True
    return mask


def best_mapping(ref: list[str | None], hyp: list[str | None], ignore: list[bool]):
    """Map hypothesised speakers onto reference speakers by co-occurrence."""
    ref_names = sorted({name for name in ref if name})
    hyp_names = sorted({name for name in hyp if name})
    if not hyp_names:
        return {}

    counts = {r: {h: 0 for h in hyp_names} for r in ref_names}
    for ref_name, hyp_name, skip in zip(ref, hyp, ignore):
        if skip or not ref_name or not hyp_name:
            continue
        counts[ref_name][hyp_name] += 1

    if len(hyp_names) <= 6:
        best_score = -1
        best: dict[str, str] = {}
        for permutation in itertools.permutations(ref_names, min(len(hyp_names), len(ref_names))):
            mapping = dict(zip(hyp_names, permutation))
            score = sum(counts[r][h] for h, r in mapping.items())
            if score > best_score:
                best_score = score
                best = mapping
        # Any hypothesis speaker left unmapped keeps its own name.
        for name in hyp_names:
            best.setdefault(name, name)
        return best

    # Greedy fallback for pathological speaker counts.
    mapping: dict[str, str] = {}
    used: set[str] = set()
    pairs = sorted(
        ((counts[r][h], r, h) for r in ref_names for h in hyp_names), reverse=True
    )
    for _, ref_name, hyp_name in pairs:
        if ref_name in used or hyp_name in mapping:
            continue
        mapping[hyp_name] = ref_name
        used.add(ref_name)
    for name in hyp_names:
        mapping.setdefault(name, name)
    return mapping


@dataclass
class Score:
    reference_seconds: float
    missed_seconds: float
    false_alarm_seconds: float
    confusion_seconds: float
    correct_seconds: float
    short_turns: int
    short_turns_found: int
    overlap_seconds: float
    mapping: dict[str, str]
    per_block: dict[int, tuple[float, float]]  # block -> (ref seconds, error seconds)
    # block -> (reference speaker, hypothesis speaker or None for missed) -> s
    block_confusion: dict[int, dict[tuple[str, str | None], float]]
    # reference turn length bucket -> confusion seconds
    bucket_confusion: dict[str, float]

    @property
    def error_seconds(self) -> float:
        return self.missed_seconds + self.false_alarm_seconds + self.confusion_seconds

    @property
    def der(self) -> float:
        return self.error_seconds / self.reference_seconds if self.reference_seconds else 0.0


def score(
    ref_turns: list[Turn],
    hyp_turns: list[Turn],
    collar: float,
    block: float,
    ignore_overlap: bool = False,
) -> Score:
    duration = max(
        [turn.end for turn in ref_turns] + [turn.end for turn in hyp_turns] + [0.0]
    )
    frames = int(duration / FRAME) + 1
    ref_sets = frame_sets(ref_turns, frames)
    # The longest covering turn drives the mapping and the confusion buckets.
    ref, ref_durations = frame_dominant(ref_turns, frames)
    hyp = frame_labels(hyp_turns, frames)
    ignore = collar_mask(ref_turns, frames, collar)
    mapping = best_mapping(ref, hyp, ignore)
    hyp = [mapping.get(name, name) if name else None for name in hyp]

    missed = false_alarm = confusion = correct = 0
    reference = 0
    overlap_frames = 0
    per_block: dict[int, list[float]] = {}
    block_confusion: dict[int, dict[tuple[str, str | None], float]] = {}
    bucket_confusion: dict[str, float] = {}

    for index, (ref_names, hyp_name) in enumerate(zip(ref_sets, hyp)):
        if ignore[index]:
            continue
        if len(ref_names) > 1:
            overlap_frames += 1
            if ignore_overlap:
                continue
        key = int(index * FRAME / block) if block > 0 else 0
        bucket = per_block.setdefault(key, [0.0, 0.0])
        if ref_names and hyp_name:
            reference += 1
            bucket[0] += FRAME
            # Every frame lands in the matrix, so the diagonal shows correct
            # seconds and the off-diagonals show confusion.
            pair = (ref[index] or "", hyp_name)
            matrix = block_confusion.setdefault(key, {})
            matrix[pair] = matrix.get(pair, 0.0) + FRAME
            # Any active reference speaker counts as correct in overlap.
            if hyp_name in ref_names:
                correct += 1
            else:
                confusion += 1
                bucket[1] += FRAME
                bucket_key = duration_bucket(ref_durations[index])
                bucket_confusion[bucket_key] = (
                    bucket_confusion.get(bucket_key, 0.0) + FRAME
                )
        elif ref_names:
            reference += 1
            bucket[0] += FRAME
            missed += 1
            bucket[1] += FRAME
            self_pair = (ref[index] or "", None)
            matrix = block_confusion.setdefault(key, {})
            matrix[self_pair] = matrix.get(self_pair, 0.0) + FRAME
        elif hyp_name:
            false_alarm += 1
            bucket[1] += FRAME

    short = [turn for turn in ref_turns if turn.duration < 1.0]
    found = 0
    for turn in short:
        first = max(0, int(turn.start / FRAME))
        last = min(frames, int(turn.end / FRAME))
        covered = sum(1 for index in range(first, last) if hyp[index] == turn.speaker)
        if last > first and covered / (last - first) >= 0.5:
            found += 1

    return Score(
        reference_seconds=reference * FRAME,
        missed_seconds=missed * FRAME,
        false_alarm_seconds=false_alarm * FRAME,
        confusion_seconds=confusion * FRAME,
        correct_seconds=correct * FRAME,
        short_turns=len(short),
        short_turns_found=found,
        overlap_seconds=overlap_frames * FRAME,
        mapping=mapping,
        per_block={key: (values[0], values[1]) for key, values in per_block.items()},
        block_confusion=block_confusion,
        bucket_confusion=bucket_confusion,
    )


def cmd_draft(args: argparse.Namespace) -> int:
    turns = read_tsv(Path(args.tsv))
    body = "\n".join(rttm_line(turn, args.file_id) for turn in turns)
    if args.out:
        Path(args.out).write_text(body + "\n", encoding="utf-8")
        print(f"wrote {args.out} ({len(turns)} turns) -- correct the speakers, then score")
    else:
        print(body)
    return 0


def cmd_from_transcript(args: argparse.Namespace) -> int:
    turns = read_transcript(
        Path(args.transcript), args.seconds_per_word, args.max_turn
    )
    if not turns:
        print(f"no timestamped lines in {args.transcript}", file=sys.stderr)
        return 2
    body = "\n".join(rttm_line(turn, args.file_id) for turn in turns)
    if args.out:
        Path(args.out).write_text(body + "\n", encoding="utf-8")
        speakers = len({turn.speaker for turn in turns})
        print(f"wrote {args.out} ({len(turns)} turns, {speakers} speaker(s))")
    else:
        print(body)
    return 0


def restrict(turns: list[Turn], start: float, end: float | None) -> list[Turn]:
    """Clip turns to a time window, so a clip can be scored against a slice."""
    clipped: list[Turn] = []
    for turn in turns:
        first = max(turn.start, start)
        last = min(turn.end, end) if end is not None else turn.end
        if last > first:
            clipped.append(Turn(first, last, turn.speaker))
    return clipped


def cmd_score(args: argparse.Namespace) -> int:
    ref = read_rttm(Path(args.ref))
    hyp = read_tsv(Path(args.hyp))
    if not ref:
        print(f"no SPEAKER lines in {args.ref}", file=sys.stderr)
        return 2
    if args.ref_range:
        start, end = args.ref_range
        ref = restrict(ref, start, end)
        if not ref:
            print(f"no reference turns in {start}-{end}s", file=sys.stderr)
            return 2
    if args.hyp_offset:
        # A run recorded on a cut clip starts at 0; shift it back onto the
        # reference timeline before comparing.
        hyp = [
            Turn(turn.start + args.hyp_offset, turn.end + args.hyp_offset, turn.speaker)
            for turn in hyp
        ]
    if args.ref_range:
        # Turns outside the scored window would otherwise count as false alarm.
        hyp = restrict(hyp, args.ref_range[0], args.ref_range[1])

    results = [
        (collar, score(ref, hyp, collar, args.block, args.ignore_overlap))
        for collar in args.collars
    ]
    primary_collar, result = results[0]
    if result.reference_seconds <= 0:
        print("reference has no speech after collars", file=sys.stderr)
        return 2

    print(f"reference speech : {result.reference_seconds:8.2f}s")
    print(f"missed speech    : {result.missed_seconds:8.2f}s  ({result.missed_seconds / result.reference_seconds:6.2%})")
    print(f"false alarm      : {result.false_alarm_seconds:8.2f}s  ({result.false_alarm_seconds / result.reference_seconds:6.2%})")
    print(f"speaker confusion: {result.confusion_seconds:8.2f}s  ({result.confusion_seconds / result.reference_seconds:6.2%})")
    print(f"correct          : {result.correct_seconds:8.2f}s  ({result.correct_seconds / result.reference_seconds:6.2%})")
    print(f"DER @{primary_collar:.2f}s      : {result.der:8.2%}")
    if len(results) > 1:
        # The reference's turn ends are estimated, so the 0.25 s headline
        # includes boundary disagreement that no diarizer can remove.
        print("DER by collar    :")
        for collar, other in results[1:]:
            print(f"  @{collar:.2f}s        : {other.der:8.2%}")
    if result.overlap_seconds:
        print(
            f"reference overlap: {result.overlap_seconds:8.2f}s"
            + ("  (excluded)" if args.ignore_overlap else "  (scored, either speaker counts)")
        )
    mapped = ", ".join(
        f"{hyp_name}->{ref_name}" for hyp_name, ref_name in sorted(result.mapping.items()) if hyp_name != ref_name
    )
    if mapped:
        print(f"speaker mapping  : {mapped}")
    if result.short_turns:
        print(
            f"short turns <1s  : {result.short_turns_found}/{result.short_turns}"
            f"  ({result.short_turns_found / result.short_turns:6.1%})"
        )
    if result.bucket_confusion and result.confusion_seconds > 0:
        print("confusion by ref turn length:")
        for bucket in ("<1s", "1-3s", ">3s"):
            seconds = result.bucket_confusion.get(bucket, 0.0)
            share = seconds / result.confusion_seconds if result.confusion_seconds else 0.0
            print(f"  {bucket:<5} {seconds:8.2f}s  ({share:6.1%})")

    if args.block > 0:
        print("per block:")
        for key in sorted(result.per_block):
            ref_seconds, error_seconds = result.per_block[key]
            der = error_seconds / ref_seconds if ref_seconds else 0.0
            print(
                f"  {int(key * args.block) // 60:5d}-{int((key + 1) * args.block) // 60:5d} min"
                f"  DER {der:6.2%}  ({ref_seconds:.1f}s reference)"
            )
            if args.matrices:
                print_block_matrix(result.block_confusion.get(key, {}))
    return 0


def print_block_matrix(matrix: dict[tuple[str, str | None], float]) -> None:
    """Ref-vs-hyp seconds for one block: catches merged and phantom speakers."""
    if not matrix:
        return
    refs = sorted({ref for ref, _ in matrix})
    hyps = sorted({hyp for _, hyp in matrix if hyp is not None})
    if len(refs) > 6 or len(hyps) > 6:
        return
    width = 12
    header = "ref\\hyp".ljust(width) + "".join(name.ljust(width) for name in hyps)
    header += "missed".rjust(width)
    print("      " + header)
    for ref_name in refs:
        line = "      " + ref_name.ljust(width)
        for hyp_name in hyps:
            line += f"{matrix.get((ref_name, hyp_name), 0.0):{width}.1f}"
        line += f"{matrix.get((ref_name, None), 0.0):{width}.1f}"
        print(line)


def run_diarize_only(binary: Path, audio: Path, speakers: int | None) -> str:
    command = [str(binary), "--cli", str(audio), "--diarize-only"]
    if speakers is not None:
        command += ["--num-speakers", str(speakers)]
    try:
        completed = subprocess.run(command, capture_output=True, text=True, check=False)
    except OSError as error:
        raise RuntimeError(f"cannot run {binary}: {error}") from error
    if completed.returncode != 0:
        raise RuntimeError(completed.stderr.strip() or f"exit {completed.returncode}")
    return completed.stdout


def cmd_record(args: argparse.Namespace) -> int:
    binary = Path(args.bin)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    failed = 0
    for name in args.audio:
        audio = Path(name)
        try:
            output = run_diarize_only(binary, audio, args.speakers)
        except RuntimeError as error:
            print(f"{audio}: {error}", file=sys.stderr)
            failed += 1
            continue
        target = out / f"{audio.stem}.tsv"
        target.write_text(output, encoding="utf-8")
        turns = len(read_tsv(target))
        print(f"{target}  ({turns} turns)")
    return 1 if failed else 0


def cmd_speakers(args: argparse.Namespace) -> int:
    turns = read_tsv(Path(args.tsv))
    names = sorted({turn.speaker for turn in turns})
    print(f"{len(names)} speaker(s): {', '.join(names) if names else '(none)'}")
    if args.expect is not None and len(names) != args.expect:
        print(
            f"expected {args.expect} speaker(s), found {len(names)}",
            file=sys.stderr,
        )
        return 1
    return 0


def cmd_compare(args: argparse.Namespace) -> int:
    golden = Path(args.golden)
    run = Path(args.run)
    changed = 0
    missing = 0
    files = sorted({path.stem for path in golden.glob("*.tsv")})
    if not files:
        print(f"no baselines in {golden}", file=sys.stderr)
        return 2
    for stem in files:
        expected_path = golden / f"{stem}.tsv"
        actual_path = run / f"{stem}.tsv"
        if not actual_path.exists():
            if args.subset:
                missing += 1
                continue
            print(f"{stem}: missing from the run")
            changed += 1
            continue
        expected = read_tsv(expected_path)
        actual = read_tsv(actual_path)
        differences = diff_turns(expected, actual, args.tolerance)
        if differences:
            changed += 1
            print(f"{stem}: {len(differences)} change(s)")
            for line in differences[:args.max_lines]:
                print(f"  {line}")
    compared = len(files) if not args.subset else len(files) - missing
    if changed:
        print(f"{changed} file(s) differ from the baseline ({compared} compared)")
        return 1
    print(f"{compared} file(s) match the baseline")
    return 0


def diff_turns(expected: list[Turn], actual: list[Turn], tolerance: float) -> list[str]:
    lines: list[str] = []
    used: set[int] = set()
    for want in expected:
        match = None
        for index, got in enumerate(actual):
            if index in used:
                continue
            if abs(got.start - want.start) <= tolerance and abs(got.end - want.end) <= tolerance:
                match = index
                break
        if match is None:
            lines.append(f"missing  {want.start:7.2f}-{want.end:7.2f}  {want.speaker}")
            continue
        used.add(match)
        got = actual[match]
        if got.speaker != want.speaker:
            lines.append(
                f"speaker  {want.start:7.2f}  {want.speaker} -> {got.speaker}"
            )
    for index, got in enumerate(actual):
        if index not in used:
            lines.append(f"added    {got.start:7.2f}-{got.end:7.2f}  {got.speaker}")
    return lines


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    from_transcript = sub.add_parser(
        "from-transcript", help="corrected transcript -> reference RTTM"
    )
    from_transcript.add_argument("transcript")
    from_transcript.add_argument("file_id")
    from_transcript.add_argument("--out")
    from_transcript.add_argument(
        "--seconds-per-word",
        type=float,
        default=0.33,
        help="speech-rate estimate used to end a turn (default 0.33)",
    )
    from_transcript.add_argument(
        "--max-turn",
        type=float,
        default=15.0,
        help="longest inferred turn in seconds (default 15)",
    )
    from_transcript.set_defaults(func=cmd_from_transcript)

    draft = sub.add_parser("draft", help="TSV turn map -> RTTM skeleton")
    draft.add_argument("tsv")
    draft.add_argument("file_id")
    draft.add_argument("--out")
    draft.set_defaults(func=cmd_draft)

    score_cmd = sub.add_parser("score", help="score a turn map against an RTTM")
    score_cmd.add_argument("--ref", required=True)
    score_cmd.add_argument("--hyp", required=True)
    score_cmd.add_argument(
        "--collars",
        type=float,
        nargs="+",
        default=[0.25, 1.0],
        help="boundary collar(s) in seconds; the first drives the detailed report",
    )
    score_cmd.add_argument("--block", type=float, default=300.0)
    score_cmd.add_argument(
        "--ref-range",
        type=float,
        nargs=2,
        metavar=("START", "END"),
        help="score only this window of the reference",
    )
    score_cmd.add_argument(
        "--hyp-offset",
        type=float,
        default=0.0,
        help="shift hypothesis times onto the reference timeline",
    )
    score_cmd.add_argument(
        "--matrices",
        action="store_true",
        help="print the ref-vs-hyp confusion matrix for each block",
    )
    score_cmd.add_argument(
        "--ignore-overlap",
        action="store_true",
        help="exclude frames with more than one reference speaker from DER",
    )
    score_cmd.set_defaults(func=cmd_score)

    speakers = sub.add_parser(
        "speakers", help="count distinct speakers in a turn map"
    )
    speakers.add_argument("tsv")
    speakers.add_argument(
        "--expect",
        type=int,
        help="exit 1 unless the turn map has exactly this many speakers",
    )
    speakers.set_defaults(func=cmd_speakers)

    record = sub.add_parser("record", help="record diarization baselines")
    record.add_argument("--bin", required=True)
    record.add_argument("--out", required=True)
    record.add_argument("--speakers", type=int)
    record.add_argument("audio", nargs="+")
    record.set_defaults(func=cmd_record)

    compare = sub.add_parser("compare", help="diff a run against recorded baselines")
    compare.add_argument("golden")
    compare.add_argument("run")
    compare.add_argument("--tolerance", type=float, default=0.05)
    compare.add_argument("--max-lines", type=int, default=20)
    compare.add_argument(
        "--subset",
        action="store_true",
        help="ignore baseline files the run did not produce",
    )
    compare.set_defaults(func=cmd_compare)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
