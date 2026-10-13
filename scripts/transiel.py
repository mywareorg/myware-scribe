#!/usr/bin/env python3
"""transiel - diarize and transcribe audio locally (CPU only)."""

import argparse
import io
import json
import os
import sys
import time
from pathlib import Path

import soundfile as sf
import torch
import torchaudio

from faster_whisper import WhisperModel

import components

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_AUDIO = REPO_ROOT / "audio" / "sample-long.mp3"

SAMPLE_RATE = 16000
LANGUAGE = "en"
MERGE_GAP_S = 1.0
MIN_TURN_S = 0.4
CLIP_PAD_S = 0.1
UNKNOWN = "UNKNOWN"

BATCH_SIZE = 32
CLUSTER_THRESHOLD = 0.6


def log(message):
    print(message, flush=True)


def format_timestamp(seconds):
    total = int(seconds)
    hours, rem = divmod(total, 3600)
    minutes, secs = divmod(rem, 60)
    return f"{hours:02d}:{minutes:02d}:{secs:02d}"


def load_audio(path, target_sr=SAMPLE_RATE):
    samples, sample_rate = sf.read(str(path), dtype="float32", always_2d=True)
    mono = samples.mean(axis=1)
    if sample_rate != target_sr:
        mono = torchaudio.functional.resample(
            torch.from_numpy(mono), sample_rate, target_sr
        ).numpy()
    return mono, target_sr


def audio_bytes(samples, sample_rate=SAMPLE_RATE):
    buffer = io.BytesIO()
    sf.write(buffer, samples, sample_rate, format="WAV")
    buffer.seek(0)
    return buffer


def embedding_arg(descriptor):
    """Map an embedding descriptor to what the pyannote fork expects.

    Passed as a ``{"checkpoint": ...}`` mapping so the fork loads the pyannote
    model checkpoint instead of misrouting the path to its ONNX loader.
    """
    loader = descriptor["loader"]
    path = components.model_path(descriptor)
    if loader == "pyannote":
        return {"checkpoint": str((path / "pytorch_model.bin").resolve())}
    raise ValueError(f"unsupported embedding loader: {loader}")


def build_config_parse(hub, segmentation_step, num_speakers):
    """DiariZen config override: force N speakers and set the window overlap.

    DiariZen's default VBxClustering ignores min/max speakers, so we switch to
    AgglomerativeClustering with min_clusters == max_clusters, which searches the
    dendrogram for the exact requested number of clusters.
    """
    import toml

    config = toml.load((Path(hub) / "config.toml").as_posix())
    inference_args = dict(config["inference"]["args"])
    inference_args["segmentation_step"] = segmentation_step

    return {
        "inference": {"args": inference_args},
        "clustering": {
            "args": {
                "method": "AgglomerativeClustering",
                "min_speakers": num_speakers,
                "max_speakers": num_speakers,
                "ahc_threshold": CLUSTER_THRESHOLD,
                "min_cluster_size": 1,
            }
        },
    }


def turns_from_annotation(annotation):
    turns = []
    for segment, _, label in annotation.itertracks(yield_label=True):
        turns.append(
            {"start": float(segment.start), "end": float(segment.end), "label": str(label)}
        )
    turns.sort(key=lambda t: (t["start"], t["end"]))
    return turns


def diarize_diarizen(samples, sample_rate, hub, embedding, tuning, num_speakers):
    from diarizen.pipelines.inference import DiariZenPipeline

    log(f"Loading DiariZen model: {hub}")
    pipeline = DiariZenPipeline(
        diarizen_hub=Path(hub),
        embedding_model=embedding,
        config_parse=build_config_parse(
            hub, tuning.get("segmentation_step", 0.1), num_speakers
        ),
    )
    log("Diarizing...")
    result = pipeline(audio_bytes(samples, sample_rate))
    return turns_from_annotation(result)


def diarize_pyannote(samples, sample_rate, segmentation, embedding, tuning, num_speakers):
    from pyannote.audio.pipelines import SpeakerDiarization

    log(f"Loading pyannote segmentation: {segmentation}")
    pipeline = SpeakerDiarization(
        segmentation=str(segmentation),
        embedding=embedding,
        clustering="AgglomerativeClustering",
        segmentation_step=tuning.get("segmentation_step", 0.1),
        embedding_batch_size=BATCH_SIZE,
        segmentation_batch_size=BATCH_SIZE,
        device=torch.device("cpu"),
    )
    pipeline.instantiate(
        {
            "segmentation": {"min_duration_off": 0.0},
            "clustering": {
                "method": "centroid",
                "min_cluster_size": 1,
                "threshold": CLUSTER_THRESHOLD,
            },
        }
    )
    log("Diarizing...")
    result = pipeline(audio_bytes(samples, sample_rate), num_speakers=num_speakers)
    return turns_from_annotation(result)


def run_diarization(samples, sample_rate, descriptor, diarizer_path, embedding, tuning, num_speakers):
    backend = descriptor["backend"]
    if backend == "diarizen":
        return diarize_diarizen(
            samples, sample_rate, diarizer_path, embedding, tuning, num_speakers
        )
    if backend == "pyannote":
        return diarize_pyannote(
            samples, sample_rate, diarizer_path, embedding, tuning, num_speakers
        )
    raise ValueError(f"unknown diarization backend: {backend}")


def clean_turns(turns, max_gap=MERGE_GAP_S, min_duration=MIN_TURN_S):
    """Merge near-adjacent same-speaker turns and drop ultra-short fragments."""
    merged = []
    for turn in sorted(turns, key=lambda t: (t["start"], t["end"])):
        if (
            merged
            and merged[-1]["label"] == turn["label"]
            and turn["start"] - merged[-1]["end"] <= max_gap
        ):
            merged[-1]["end"] = max(merged[-1]["end"], turn["end"])
        else:
            merged.append(dict(turn))
    return [t for t in merged if t["end"] - t["start"] >= min_duration]


def build_speaker_map(turns):
    mapping = {}
    for turn in turns:
        label = turn["label"]
        if label not in mapping:
            mapping[label] = f"SPEAKER {len(mapping) + 1}"
    return mapping


def load_whisper(model_dir, device, compute_type):
    if not Path(model_dir).is_dir():
        sys.exit(f"whisper model not found: {model_dir}")
    log(f"Loading whisper model: {model_dir}")
    return WhisperModel(str(model_dir), device=device, compute_type=compute_type)


def transcribe_turns(model, samples, sample_rate, turns, beam_size, pad=CLIP_PAD_S):
    """Transcribe each speaker turn independently.

    Speaker labels come from the diarization turn, so transcription timing never
    influences speaker assignment.
    """
    log("Transcribing...")
    total = len(samples)
    for turn in turns:
        start = max(0.0, turn["start"] - pad)
        end = min(total / sample_rate, turn["end"] + pad)
        clip = samples[int(start * sample_rate) : int(end * sample_rate)]
        if len(clip) < int(0.15 * sample_rate):
            turn["text"] = ""
            continue
        segments, _ = model.transcribe(
            clip,
            language=LANGUAGE,
            beam_size=beam_size,
            condition_on_previous_text=False,
            word_timestamps=False,
        )
        turn["text"] = " ".join(segment.text.strip() for segment in segments).strip()
    return turns


def build_lines(turns, mapping):
    lines = []
    for turn in turns:
        text = turn.get("text", "").strip()
        if not text:
            continue
        lines.append(
            {
                "time": format_timestamp(turn["start"]),
                "start": turn["start"],
                "end": turn["end"],
                "speaker": mapping.get(turn["label"], UNKNOWN),
                "text": text,
            }
        )
    return lines


def main():
    parser = argparse.ArgumentParser(
        prog="transiel",
        description="Diarize and transcribe audio locally (CPU only).",
        epilog=(
            "components: run 'python scripts/components.py --list' to see names"
        ),
    )
    parser.add_argument(
        "audio",
        nargs="?",
        default=str(DEFAULT_AUDIO),
        help="audio file to process (default: audio/sample-long.mp3)",
    )
    preset_group = parser.add_mutually_exclusive_group()
    preset_group.add_argument(
        "--preset",
        choices=list(components.PRESETS),
        default=None,
        help=f"component preset (default: {components.DEFAULT_PRESET})",
    )
    preset_group.add_argument(
        "--speed",
        dest="preset",
        action="store_const",
        const="speed",
        help="shortcut for --preset speed",
    )
    preset_group.add_argument(
        "--balanced",
        dest="preset",
        action="store_const",
        const="balanced",
        help="shortcut for --preset balanced",
    )
    preset_group.add_argument(
        "--accuracy",
        dest="preset",
        action="store_const",
        const="accuracy",
        help="shortcut for --preset accuracy (default)",
    )
    parser.add_argument("--diarizer", help="diarizer component name (overrides preset)")
    parser.add_argument("--embedding", help="embedding component name (overrides preset)")
    parser.add_argument("--transcriber", help="transcriber component name (overrides preset)")
    parser.add_argument(
        "--whisper-model",
        default=None,
        help="raw path to a faster-whisper model directory (overrides transcriber)",
    )
    parser.add_argument(
        "--diarizen-hub",
        default=None,
        help="raw path to the diarization model directory (overrides diarizer)",
    )
    parser.add_argument(
        "--diarizen-embedding",
        default=None,
        help="raw path to the embedding checkpoint (overrides embedding)",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="output basename (default: <audio-stem>-transcribe)",
    )
    parser.add_argument("--device", default="cpu", help="torch/ct2 device (default: cpu)")
    parser.add_argument(
        "--compute-type", default="int8", help="ct2 compute type (default: int8)"
    )
    args = parser.parse_args()

    try:
        resolved = components.resolve(
            args.preset or components.DEFAULT_PRESET,
            args.diarizer,
            args.embedding,
            args.transcriber,
        )
    except KeyError as exc:
        sys.exit(str(exc))

    audio_path = Path(args.audio).resolve()
    if not audio_path.is_file():
        sys.exit(f"audio file not found: {audio_path}")

    output_base = Path(args.output) if args.output else REPO_ROOT / f"{audio_path.stem}-transcribe"
    out_txt = output_base.with_suffix(".txt")
    out_json = output_base.with_suffix(".json")

    tuning = resolved["tuning"]
    threads = tuning.get("threads") or os.cpu_count() or 1
    torch.set_num_threads(threads)
    beam_size = tuning.get("beam_size", 5)

    diarizer_path = (
        Path(args.diarizen_hub)
        if args.diarizen_hub
        else components.model_path(resolved["diarizer"])
    )
    embedding = (
        {"checkpoint": str(Path(args.diarizen_embedding).resolve())}
        if args.diarizen_embedding
        else embedding_arg(resolved["embedding"])
    )
    whisper_dir = Path(args.whisper_model) if args.whisper_model else components.model_path(resolved["transcriber"])

    log(f"Preset: {resolved['preset']}")
    log(f"  diarizer:   {resolved['diarizer_name']} ({resolved['diarizer']['backend']})")
    log(f"  embedding:  {resolved['embedding_name']}")
    log(f"  transcriber:{resolved['transcriber_name']}")
    log(f"Audio: {audio_path}")

    timings = {}

    start = time.perf_counter()
    samples, sample_rate = load_audio(audio_path)
    duration = len(samples) / sample_rate
    timings["load_audio_s"] = round(time.perf_counter() - start, 3)
    log(f"Duration: {format_timestamp(duration)}")

    start = time.perf_counter()
    raw_turns = run_diarization(
        samples,
        sample_rate,
        resolved["diarizer"],
        diarizer_path,
        embedding,
        tuning,
        components.NUM_SPEAKERS,
    )
    turns = clean_turns(raw_turns)
    timings["diarization_s"] = round(time.perf_counter() - start, 3)
    speaker_map = build_speaker_map(turns)
    log(
        f"Detected {len(speaker_map)} speaker(s), "
        f"{len(turns)} turn(s) ({len(raw_turns)} raw)"
    )

    start = time.perf_counter()
    model = load_whisper(whisper_dir, args.device, args.compute_type)
    turns = transcribe_turns(model, samples, sample_rate, turns, beam_size)
    timings["transcription_s"] = round(time.perf_counter() - start, 3)

    lines = build_lines(turns, speaker_map)

    txt = "".join(f"[{line['time']}] {line['speaker']}: {line['text']}\n" for line in lines)
    out_txt.write_text(txt, encoding="utf-8")

    payload = {
        "audio": {"path": str(audio_path), "duration": duration},
        "components": {
            "preset": resolved["preset"],
            "diarizer": resolved["diarizer_name"],
            "embedding": resolved["embedding_name"],
            "transcriber": resolved["transcriber_name"],
        },
        "models": {
            "whisper": str(whisper_dir.resolve()),
            "diarization": str(Path(diarizer_path).resolve()),
        },
        "run": {
            "device": args.device,
            "compute_type": args.compute_type,
            "num_speakers": components.NUM_SPEAKERS,
            "beam_size": beam_size,
            "tuning": tuning,
            "merge_gap_s": MERGE_GAP_S,
            "min_turn_s": MIN_TURN_S,
            "clip_pad_s": CLIP_PAD_S,
            "timings": timings,
        },
        "speakers": speaker_map,
        "diarization": turns,
        "lines": lines,
    }
    out_json.write_text(json.dumps(payload, indent=2, ensure_ascii=False), encoding="utf-8")

    log("")
    for line in lines:
        log(f"[{line['time']}] {line['speaker']}: {line['text']}")

    log("")
    log(f"Wrote {out_txt}")
    log(f"Wrote {out_json}")


if __name__ == "__main__":
    main()
