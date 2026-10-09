#!/usr/bin/env python3
"""transiel - diarize and transcribe audio locally (CPU only)."""

import argparse
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import torch
import torchaudio

from faster_whisper import WhisperModel
from pyannote.audio import Pipeline
from pyannote.audio.pipelines.utils.hook import ProgressHook

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = REPO_ROOT / "models"
DEFAULT_AUDIO = REPO_ROOT / "audio" / "sample-long.mp3"
DEFAULT_WHISPER = MODELS_DIR / "faster-whisper-base.en"
DEFAULT_DIARIZATION = MODELS_DIR / "speaker-diarization-community-1"
DEFAULT_DIARIZEN_HUB = MODELS_DIR / "diarizen-wavlm-large-s80-md-v2"
DEFAULT_DIARIZEN_EMBEDDING = MODELS_DIR / "wespeaker-voxceleb-resnet34-LM" / "pytorch_model.bin"
DEFAULT_DIARIZEN_PYTHON = REPO_ROOT / ".venv-diarizen" / "bin" / "python"
DIARIZEN_RUNNER = REPO_ROOT / "scripts" / "diarizen_diarize.py"

UNKNOWN = "UNKNOWN"


def log(message):
    print(message, flush=True)


def format_timestamp(seconds):
    total = int(seconds)
    hours, rem = divmod(total, 3600)
    minutes, secs = divmod(rem, 60)
    return f"{hours:02d}:{minutes:02d}:{secs:02d}"


def load_audio(path, target_sr=16000):
    waveform, sample_rate = torchaudio.load(str(path))
    if waveform.shape[0] > 1:
        waveform = waveform.mean(dim=0, keepdim=True)
    if sample_rate != target_sr:
        waveform = torchaudio.functional.resample(waveform, sample_rate, target_sr)
    return waveform, target_sr


def run_diarization_diarizen(audio_path, hub, embedding, python, num_speakers, min_speakers, max_speakers):
    if not Path(python).is_file():
        sys.exit(
            f"DiariZen interpreter not found: {python}\n"
            "Run scripts/setup-diarizen.sh to create the dedicated environment."
        )
    if not Path(hub).is_dir():
        sys.exit(f"DiariZen hub not found: {hub}")
    if not Path(embedding).is_file():
        sys.exit(
            f"embedding model not found: {embedding}\n"
            "Run scripts/model-download.py to fetch pyannote/wespeaker-voxceleb-resnet34-LM."
        )

    log(f"Loading DiariZen model: {hub}")
    with tempfile.TemporaryDirectory() as tmp:
        turns_json = Path(tmp) / "turns.json"
        cmd = [
            str(python),
            str(DIARIZEN_RUNNER),
            "--hub", str(hub),
            "--embedding", str(embedding),
            "--audio", str(audio_path),
            "--output-json", str(turns_json),
        ]
        if num_speakers is not None:
            cmd += ["--num-speakers", str(num_speakers)]
        else:
            if min_speakers is not None:
                cmd += ["--min-speakers", str(min_speakers)]
            if max_speakers is not None:
                cmd += ["--max-speakers", str(max_speakers)]

        log("Diarizing (DiariZen)...")
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode != 0:
            sys.exit(f"DiariZen diarization failed:\n{result.stderr}")
        turns = json.loads(turns_json.read_text(encoding="utf-8"))["turns"]

    turns.sort(key=lambda t: (t["start"], t["end"]))
    return turns


def run_diarization(audio, model_dir, num_speakers, min_speakers, max_speakers):
    if not Path(model_dir).is_dir():
        sys.exit(f"diarization model not found: {model_dir}")

    log(f"Loading diarization model: {model_dir}")
    pipeline = Pipeline.from_pretrained(str(model_dir))
    pipeline.to(torch.device("cpu"))
    kwargs = {}
    if num_speakers is not None:
        kwargs["num_speakers"] = num_speakers
    else:
        if min_speakers is not None:
            kwargs["min_speakers"] = min_speakers
        if max_speakers is not None:
            kwargs["max_speakers"] = max_speakers

    log("Diarizing...")
    with ProgressHook() as hook:
        output = pipeline(audio, hook=hook)

    annotation = getattr(output, "exclusive_speaker_diarization", None)
    if annotation is None:
        annotation = getattr(output, "speaker_diarization", output)

    turns = []
    for segment, _, label in annotation.itertracks(yield_label=True):
        turns.append({"start": segment.start, "end": segment.end, "label": label})
    turns.sort(key=lambda t: (t["start"], t["end"]))
    return turns


def build_speaker_map(turns):
    mapping = {}
    for turn in turns:
        label = turn["label"]
        if label not in mapping:
            mapping[label] = f"SPEAKER {len(mapping) + 1}"
    return mapping


def assign_speaker(segment, turns, mapping):
    best_label = None
    best_overlap = 0.0
    for turn in turns:
        overlap = min(segment["end"], turn["end"]) - max(segment["start"], turn["start"])
        if overlap > best_overlap:
            best_overlap = overlap
            best_label = turn["label"]

    if best_label is not None:
        return mapping.get(best_label, UNKNOWN)

    nearest_label = None
    nearest_distance = None
    for turn in turns:
        if turn["end"] < segment["start"]:
            distance = segment["start"] - turn["end"]
        elif turn["start"] > segment["end"]:
            distance = turn["start"] - segment["end"]
        else:
            distance = 0.0
        if nearest_distance is None or distance < nearest_distance:
            nearest_distance = distance
            nearest_label = turn["label"]

    return mapping.get(nearest_label, UNKNOWN) if nearest_label else UNKNOWN


def run_transcription(samples, model_dir, device, compute_type):
    if not Path(model_dir).is_dir():
        sys.exit(f"whisper model not found: {model_dir}")

    log(f"Loading whisper model: {model_dir}")
    model = WhisperModel(str(model_dir), device=device, compute_type=compute_type)

    log("Transcribing...")
    segments_iter, info = model.transcribe(samples, language="en")

    segments = []
    for seg in segments_iter:
        segments.append(
            {
                "start": seg.start,
                "end": seg.end,
                "text": seg.text.strip(),
                "avg_logprob": seg.avg_logprob,
                "no_speech_prob": seg.no_speech_prob,
                "compression_ratio": seg.compression_ratio,
                "temperature": seg.temperature,
            }
        )

    metadata = {
        "language": info.language,
        "language_probability": info.language_probability,
        "duration": info.duration,
        "duration_after_vad": info.duration_after_vad,
    }
    return segments, metadata


def main():
    parser = argparse.ArgumentParser(
        prog="transiel",
        description="Diarize and transcribe audio locally (CPU only).",
    )
    parser.add_argument(
        "audio",
        nargs="?",
        default=str(DEFAULT_AUDIO),
        help="audio file to process (default: audio/sample-long.mp3)",
    )
    parser.add_argument(
        "--whisper-model",
        default=str(DEFAULT_WHISPER),
        help="path to a faster-whisper (CTranslate2) model directory",
    )
    parser.add_argument(
        "--backend",
        choices=["diarizen", "community"],
        default="diarizen",
        help="diarization backend (default: diarizen)",
    )
    parser.add_argument(
        "--diarization-model",
        default=str(DEFAULT_DIARIZATION),
        help="path to a pyannote diarization pipeline directory (community backend)",
    )
    parser.add_argument(
        "--diarizen-hub",
        default=str(DEFAULT_DIARIZEN_HUB),
        help="path to the DiariZen model directory",
    )
    parser.add_argument(
        "--diarizen-embedding",
        default=str(DEFAULT_DIARIZEN_EMBEDDING),
        help="path to the DiariZen speaker embedding model file",
    )
    parser.add_argument(
        "--diarizen-python",
        default=str(DEFAULT_DIARIZEN_PYTHON),
        help="interpreter of the dedicated DiariZen environment",
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
    parser.add_argument("--num-speakers", type=int, default=2)
    parser.add_argument("--min-speakers", type=int, default=None)
    parser.add_argument("--max-speakers", type=int, default=None)
    args = parser.parse_args()

    audio_path = Path(args.audio).resolve()
    if not audio_path.is_file():
        sys.exit(f"audio file not found: {audio_path}")

    output_base = Path(args.output) if args.output else REPO_ROOT / f"{audio_path.stem}-transcribe"
    out_txt = output_base.with_suffix(".txt")
    out_json = output_base.with_suffix(".json")

    timings = {}

    log(f"Audio: {audio_path}")

    start = time.perf_counter()
    samples, sample_rate = load_audio(audio_path)
    duration = samples.shape[1] / sample_rate
    timings["load_audio_s"] = round(time.perf_counter() - start, 3)
    log(f"Duration: {format_timestamp(duration)}")

    start = time.perf_counter()
    if args.backend == "diarizen":
        turns = run_diarization_diarizen(
            audio_path,
            args.diarizen_hub,
            args.diarizen_embedding,
            args.diarizen_python,
            args.num_speakers,
            args.min_speakers,
            args.max_speakers,
        )
    else:
        turns = run_diarization(
            {"waveform": samples, "sample_rate": sample_rate},
            args.diarization_model,
            args.num_speakers,
            args.min_speakers,
            args.max_speakers,
        )
    timings["diarization_s"] = round(time.perf_counter() - start, 3)
    speaker_map = build_speaker_map(turns)
    log(f"Detected {len(speaker_map)} speaker(s), {len(turns)} turn(s)")

    start = time.perf_counter()
    segments, whisper_meta = run_transcription(
        samples[0].numpy(), args.whisper_model, args.device, args.compute_type
    )
    timings["transcription_s"] = round(time.perf_counter() - start, 3)

    for segment in segments:
        segment["speaker"] = assign_speaker(segment, turns, speaker_map)

    lines = [
        {
            "time": format_timestamp(segment["start"]),
            "start": segment["start"],
            "end": segment["end"],
            "speaker": segment["speaker"],
            "text": segment["text"],
        }
        for segment in segments
    ]

    txt = "".join(f"[{line['time']}] {line['speaker']}: {line['text']}\n" for line in lines)
    out_txt.write_text(txt, encoding="utf-8")

    payload = {
        "audio": {"path": str(audio_path), "duration": duration},
        "models": {
            "whisper": str(Path(args.whisper_model).resolve()),
            "diarization": (
                str(Path(args.diarizen_hub).resolve())
                if args.backend == "diarizen"
                else str(Path(args.diarization_model).resolve())
            ),
        },
        "run": {
            "backend": args.backend,
            "device": args.device,
            "compute_type": args.compute_type,
            "num_speakers": args.num_speakers,
            "min_speakers": args.min_speakers,
            "max_speakers": args.max_speakers,
            "timings": timings,
            "whisper": whisper_meta,
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
