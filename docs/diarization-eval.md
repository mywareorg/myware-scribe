# Diarization evaluation

Speaker isolation cannot be judged from a transcript alone: a run that loses a
short interjection and a run that invents a second speaker both "look fine" in
the text. This is the harness that turns those changes into numbers.

The tool is `scripts/diarize-score.py` (standard library only).

## Building a reference (preferred: correct in the app)

Do not hand-edit an RTTM. Transcribe in MyWare Transcribe, fix the speakers there,
and derive the reference from the corrected transcript.

1. Open the audio in the app and transcribe it (Balanced is fine).
2. Fix the speakers on the transcript:
   - double-click a line to flip it to the other speaker,
   - right-click to edit a line, merge it with a neighbour, or split it.
   Text edits do not matter for scoring; the speaker column and the timestamps do.
3. Save As → keep the timestamped form (`[h:mm:ss] SPEAKER n: text`). The app
   writes this as `<stem>-ts.txt`.
4. Convert it to an RTTM:

   ```
   scripts/diarize-score.py from-transcript \
       audio/sample-long-accuracy-transcription-2speakers-ts-corrected.txt \
       sample-long --out tests/diarization/sample-long.rttm
   ```

   Each line becomes a turn that starts at its timestamp and ends at the smaller
   of the next line's start and a words-per-second estimate
   (`--seconds-per-word`, `--max-turn`).

5. Score a diarization run against it (see below).

### Overlapping speech

A transcript line carries one speaker, so overlap is encoded structurally: write
**two lines with the same timestamp**, one per speaker, each holding the words
that speaker said. Only do this where both voices are actually audible; otherwise
keep the dominant speaker's line.

`from-transcript` keeps same-timestamp lines at their own word-rate estimate
instead of collapsing them, so the overlap survives into the RTTM as two
overlapping turns. The scorer then:

- reports `reference overlap: N s`,
- counts a hypothesis frame as correct if it matches *any* speaker active in that
  frame,
- offers `--ignore-overlap` to drop those frames from DER entirely (the usual
  convention when the system under test does not emit overlap).

The current diarizer never emits overlapping turns, so overlap always shows up
as the "either speaker counts" case rather than as a false alarm.

What this reference measures, and what it does not:

- It captures **speaker attribution**: whether the right person is credited for
  each transcribed line. That is the error that shows up in the transcript.
- It does **not** capture exact turn timing. Boundaries come from ASR line starts
  and an estimate, so missed speech inside a line and precise onset/offset
  differences are invisible. Treat DER from this reference as a speaker-
  attribution score, not a NIST number.
- For an absolute DER on a small stretch, hand-check a few minutes against the
  audio and note it separately.

## Alternative: label from a diarization draft

When no corrected transcript exists, a draft RTTM is still a useful starting
point for hand correction:

```
scripts/diarize-score.py record --bin target/debug/myware-transcribe \
    --speakers 2 --out audio/eval/baseline <audio>
scripts/diarize-score.py draft audio/eval/baseline/<stem>.tsv <stem> \
    --out tests/diarization/<stem>.rttm
```

Drafts contain nested turns (a long dominant-speaker turn with the other
speaker's interjections inside it); split the long turn around them when
correcting.

Note: `--profile release-fast` cannot build this workspace -- the sherpa-onnx
build script requires the output directory to be named after `PROFILE`. Use the
debug or release profile.

## Scoring a change

```
scripts/diarize-score.py score --ref tests/diarization/sample-long.rttm \
    --hyp audio/eval/baseline/sample-long.tsv --matrices
```

Reported:

- reference speech duration after the collar
- missed speech, false alarm, speaker confusion, correct, each as seconds and %
- DER = (missed + false alarm + confusion) / reference speech
- **DER at every requested collar** (`--collars`, default 0.25 and 1.0). The
  reference's turn ends are estimates, so the 0.25 s figure includes boundary
  disagreement no diarizer can remove. Quote both.
- recall of reference turns shorter than 1 s (covered ≥ 50% by the matching speaker)
- **confusion by reference turn length** (`<1s`, `1-3s`, `>3s`), which is what
  tells short-interjection errors apart from whole-turn misassignment
- DER per block (default 300 s), and with `--matrices` the ref-vs-hyp seconds
  matrix for each block. The diagonal is correct speech; `missed` is its own
  column. This is the only view that shows one speaker being absorbed by the
  other.
- the hypothesis→reference speaker mapping that was applied

Frames are 10 ms. The mapping and the length buckets use the **longest covering
reference turn** per frame, so both stay stable under overlap. Overlapping
reference speech still counts as correct for either active speaker.

`--ref-range START END` scores only a window of the reference, and
`--hyp-offset S` shifts a clip's timestamps onto that window. Together they let
a 5-minute cut be compared against the matching slice of a longer reference.

### Phantom-speaker check

A known single-speaker file must produce one label:

```
scripts/diarize-score.py record --bin target/debug/myware-transcribe \
    --speakers 2 --out /tmp/mono audio/monologue.mp3
scripts/diarize-score.py speakers /tmp/mono/monologue.tsv --expect 1
```

`--expect` exits 1 on a mismatch. There is no single-speaker file in the repo
yet; add one when a clean monologue is available, because this is the only
direct test of the phantom-speaker failure mode.

## Regression gate

Every pipeline change should reproduce the baseline turn map exactly unless it
was meant to change it:

```
scripts/diarize-score.py record --bin target/debug/myware-transcribe --out /tmp/run \
    audio/hour-conversation-short-1.mp3
scripts/diarize-score.py compare audio/eval/baseline /tmp/run --subset
```

`compare` exits 1 when a turn was added, removed, or changed speaker (start and
end matched within 50 ms). `--subset` lets you check one file without failing on
the baselines the run did not produce. That is the cheap guard; `score` is the
quality guard.

`--diarize-only` applies the pipeline's own `clean_turns` and prints a leading
`#` line with the stage timing and cleanup counts, so a recorded baseline matches
the geometry the app ships and a run reports its own diarization cost:

```
# diarize 31.34s audio 600.0s threads 8 raw 91 merged 4 dropped 9 kept 78 speaker(s) 2
```

`read_tsv` skips `#` lines, so the same files work as baselines.

## Not covered yet

- Speaker-attributed WER: needs a reference transcript with speaker labels.
- ASR timing beside diarization timing: the `diarize < ASR wall time` gate needs a
  full pipeline run, not `--diarize-only`.
- Overlapping speech as first-class output: the RTTM reader accepts overlaps but
  scoring collapses them per frame.
- Automatic threshold tuning: change one parameter at a time and compare `score`
  output; do not tune against a single file.
