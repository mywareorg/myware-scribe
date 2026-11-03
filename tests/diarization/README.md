# Diarization references

`sample-long.rttm` is a real reference: it was derived from a transcript
corrected in the app (`audio/sample-long-accuracy-transcription-2speakers-ts-corrected.txt`,
233 lines, 2 speakers) with

```
scripts/diarize-score.py from-transcript \
    audio/sample-long-accuracy-transcription-2speakers-ts-corrected.txt \
    sample-long --out tests/diarization/sample-long.rttm
```

Its boundaries are ASR line starts, capped by a words-per-second estimate, so it
measures speaker attribution rather than exact turn timing.

The other files are **drafts** generated from the current diarizer:

- `hour-conversation-short-1.rttm` — 2 minutes, 21 turns.
- `hour-conversation-mid-1.rttm` — 5 minutes, 78 turns.
- `sample-short.rttm` — 2 minutes, 9 turns.

They exist only as something to correct. Every draft contains nested turns: a
long dominant-speaker turn with the other speaker's short turns inside it. Split
the long turn around the interjections rather than deleting them.

Preferred replacement: correct the speakers in the app and regenerate with
`from-transcript` (see `docs/diarization-eval.md`), then delete the draft.

Do not regenerate a reference from the pipeline *after* changing the diarizer —
that makes the reference agree with the system under test.

Overlap is encoded in the source transcript as two lines sharing a timestamp;
`from-transcript` turns those into two overlapping RTTM turns.

Score with:

```
scripts/diarize-score.py score --ref tests/diarization/sample-long.rttm \
    --hyp audio/eval/baseline/sample-long.tsv --matrices
```

## Recorded baseline

10 minutes, debug build, balanced preset, `--num-speakers 2`, and -- since
`--diarize-only` now applies the pipeline's own `clean_turns` -- the turn
geometry the app actually ships:

```
reference speech : 435.08s
missed speech    :   4.72s ( 1.08%)
false alarm      :  20.29s ( 4.66%)
speaker confusion:  31.73s ( 7.29%)
correct          : 398.63s (91.62%)
DER @0.25s       :  13.04%
DER @1.00s       :   7.08%
reference overlap:   4.56s
short turns <1s  : 31/58 (53.4%)
confusion by ref turn length: <1s 2.98s, 1-3s 8.62s, >3s 20.13s
per block        : 10.23% (0-5 min), 16.10% (5-10 min)
diarize stage    : 31.34s for 600s audio (0.052x realtime, 8 threads)
```

The ref-vs-hyp matrices:

```
0-5 min            hyp S1     hyp S2   missed
ref SPEAKER 1      168.5        3.7      0.5
ref SPEAKER 2       10.7       43.4      0.0
5-10 min
ref SPEAKER 1      112.6        5.3      1.8
ref SPEAKER 2       13.5       72.7      2.5
```

What this says, and it is not what the original plan assumed:

- **Most of the error is one-directional**: SPEAKER 2's speech is credited to
  SPEAKER 1 for ~24 s across the file. That is a merge/absorption error, which
  is what cluster-level reassignment is meant to fix.
- **It is not drift.** The two blocks lose about the same amount of SPEAKER 2
  (10.7 s vs 13.5 s). The block DER difference comes from false alarm, not from
  the second half degrading.
- **It is not primarily short turns.** Only 9.4% of confusion sits inside
  reference turns under 1 s; 63.4% sits inside turns over 3 s.
- **Half the headline is boundary noise.** 13.04% at a 0.25 s collar is 7.08% at
  1.0 s; the reference's turn ends are word-rate estimates, not measured.

`--collars` reports both by default. `--ref-range START END` scores a window of
the reference and `--hyp-offset S` shifts a clip's turns onto it, so a 5-minute
cut can be compared against the matching slice.

## First decisive comparison (2026-11-03)

Same 300-600 s window of `sample-long`, same reference slice, two hypotheses:

```
                        DER@0.25   DER@1.0   confusion   S2 -> S1
full-file single pass    16.10%     10.27%     17.51s       13.5s
5-min clip, alone        12.56%      4.86%     10.21s        5.7s
```

Reproduce with:

```
ffmpeg -ss 300 -t 300 -i audio/sample-long.mp3 -ac 1 -ar 16000 /tmp/clip.wav
scripts/diarize-score.py record --bin target/debug/hippo-transcribe --speakers 2 \
    --out /tmp/clip /tmp/clip.wav
scripts/diarize-score.py score --ref tests/diarization/sample-long.rttm \
    --hyp /tmp/clip/clip.tsv --ref-range 300 600 --hyp-offset 300
scripts/diarize-score.py score --ref tests/diarization/sample-long.rttm \
    --hyp audio/eval/baseline/sample-long.tsv --ref-range 300 600
```

Local diarization halves the one-directional absorption on the same audio, so
the global clustering step is the thing to replace. That is the Phase 3 target,
and it is measured, not assumed.
