#!/usr/bin/env python3
"""transiel - diarize and transcribe audio locally (CPU only)."""

import argparse
import gc
import importlib
import io
import json
import math
import os
import sys
import threading
import time
import types
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path


def _physical_cores():
    try:
        import psutil

        count = psutil.cpu_count(logical=False)
        if count:
            return count
    except Exception:  # noqa: BLE001
        pass
    return max(1, (os.cpu_count() or 2) // 2)


# Thread counts must be set before numpy/torch/CTranslate2 are imported so their
# thread pools pick them up.
PHYSICAL_CORES = _physical_cores()
for _var in (
    "OMP_NUM_THREADS",
    "MKL_NUM_THREADS",
    "OPENBLAS_NUM_THREADS",
    "NUMEXPR_NUM_THREADS",
    "VECLIB_MAXIMUM_THREADS",
):
    os.environ.setdefault(_var, str(PHYSICAL_CORES))

import psutil  # noqa: E402
import soundfile as sf  # noqa: E402
import torch  # noqa: E402
import torchaudio  # noqa: E402
from faster_whisper import WhisperModel  # noqa: E402

import components  # noqa: E402

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

# Progress (fraction of the whole run) boundaries for each stage.
STAGE_BOUNDS = {
    "loading": (0.00, 0.05),
    "diarization": (0.05, 0.65),
    "transcription": (0.65, 0.95),
    "writing": (0.95, 1.00),
}

PROGRESS_JSON = False


def emit(event):
    """Emit a machine-readable progress event on stdout (when --progress-json)."""
    if PROGRESS_JSON:
        print(json.dumps(event), flush=True)


def emit_stage(stage, progress, label=None):
    emit(
        {
            "event": "stage",
            "stage": stage,
            "label": label if label is not None else stage,
            "progress": round(float(progress), 4),
        }
    )


def start_creep(stage, label, stop):
    """Emit time-smoothed progress for a stage with no measurable sub-progress.

    Approaches the stage's upper bound asymptotically (capped below it) so the
    bar keeps moving without overrunning the real completion.
    """
    lo, hi = STAGE_BOUNDS[stage]

    def run():
        start = time.perf_counter()
        tau = 25.0
        while not stop.wait(0.5):
            frac = 1.0 - math.exp(-(time.perf_counter() - start) / tau)
            emit_stage(stage, lo + (hi - lo) * frac * 0.9, label)

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread


def detect_backends():
    """Available torch accelerators, with the vendor name (cuda vs rocm)."""
    backends = []
    if torch.cuda.is_available():
        backends.append("rocm" if getattr(torch.version, "hip", None) else "cuda")
    if getattr(torch.backends, "mps", None) and torch.backends.mps.is_available():
        backends.append("mps")
    if hasattr(torch, "xpu") and torch.xpu.is_available():
        backends.append("xpu")
    backends.append("cpu")
    return backends


def resolve_device(name):
    """Resolve a device name to a torch.device ("auto" picks the best available)."""
    if name == "auto":
        if torch.cuda.is_available():
            return torch.device("cuda")
        if getattr(torch.backends, "mps", None) and torch.backends.mps.is_available():
            return torch.device("mps")
        if hasattr(torch, "xpu") and torch.xpu.is_available():
            return torch.device("xpu")
        return torch.device("cpu")
    return torch.device(name)


def transcription_device(device):
    """Device for faster-whisper. CTranslate2 supports CUDA (NVIDIA) only, so
    AMD ROCm / Apple MPS / Intel XPU fall back to CPU for transcription."""
    if device.type == "cuda" and getattr(torch.version, "hip", None) is None:
        return "cuda"
    return "cpu"


def auto_tuning(transcriber, transcribe_device, available_mb, total_mb, physical_cores):
    """Pick batch size / transcription workers from the available hardware.

    Keeps a headroom for the OS/desktop, and never replicates a model more times
    than the memory budget allows (the main OOM risk on big models).
    """
    model_mb = int(transcriber.get("mem_mb") or 512)
    # Per-worker cost = model + working buffers.
    per_worker_mb = model_mb + 512
    headroom_mb = max(4096, int(0.30 * total_mb))
    budget_mb = max(0, available_mb - headroom_mb)

    if transcribe_device == "cuda":
        workers = 1
    else:
        workers = max(1, min(physical_cores, budget_mb // max(1, per_worker_mb)))
    transcribe_threads = max(1, physical_cores // workers)

    if budget_mb < 4096:
        batch_size = 16
    elif budget_mb < 8192:
        batch_size = 32
    else:
        batch_size = 64

    return {
        "transcribe_workers": int(workers),
        "transcribe_threads": int(transcribe_threads),
        "batch_size": int(batch_size),
    }


def log(message):
    print(message, file=sys.stderr if PROGRESS_JSON else sys.stdout, flush=True)


def format_timestamp(seconds, use_hours=False):
    total = int(seconds)
    hours, rem = divmod(total, 3600)
    minutes, secs = divmod(rem, 60)
    if use_hours or hours:
        return f"{hours:02d}:{minutes:02d}:{secs:02d}"
    return f"{minutes:02d}:{secs:02d}"


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


def build_config_parse(hub, segmentation_step, batch_size, num_speakers):
    """DiariZen config override: force N speakers and set the window overlap.

    DiariZen's default VBxClustering ignores min/max speakers, so we switch to
    AgglomerativeClustering with min_clusters == max_clusters, which searches the
    dendrogram for the exact requested number of clusters.
    """
    import toml

    config = toml.load((Path(hub) / "config.toml").as_posix())
    inference_args = dict(config["inference"]["args"])
    inference_args["segmentation_step"] = segmentation_step
    inference_args["batch_size"] = batch_size

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
            hub,
            tuning.get("segmentation_step", 0.1),
            tuning.get("batch_size", BATCH_SIZE),
            num_speakers,
        ),
    )
    log("Diarizing...")
    result = pipeline(audio_bytes(samples, sample_rate))
    turns = turns_from_annotation(result)
    # Release the diarization model/buffers before Whisper is loaded.
    del pipeline, result
    gc.collect()
    if torch.cuda.is_available():
        torch.cuda.empty_cache()
    return turns


def load_pyannote_segmentation(seg_dir):
    """Load a stock pyannote segmentation model with the DiariZen pyannote fork.

    The fork's ``Model`` is a plain ``nn.Module`` and lacks Lightning's
    ``save_hyperparameters``/``hparams`` that pyannote segmentation classes
    (e.g. ``PyanNet``) rely on. We shim those, build the model from its
    ``config.yaml``, fix the powerset class count to match the checkpoint, and
    load the weights directly.
    """
    import yaml

    from pyannote.audio.core.task import Problem, Resolution, Specifications
    from pyannote.audio.utils.powerset import Powerset

    seg_dir = Path(seg_dir)
    config = yaml.safe_load((seg_dir / "config.yaml").read_text(encoding="utf-8"))

    model_cfg = dict(config["model"])
    target = model_cfg.pop("_target_")
    module_name, class_name = target.rsplit(".", 1)
    klass = getattr(importlib.import_module(module_name), class_name)

    # Scope the shim to the segmentation class only (applying it to the shared
    # base Model would shadow pytorch-lightning's save_hyperparameters for the
    # WeSpeaker embedding, which relies on it).
    if not hasattr(klass, "save_hyperparameters"):

        def save_hyperparameters(self, *names):
            locals_ = sys._getframe(1).f_locals
            hparams = self.__dict__.get("hparams")
            if hparams is None:
                hparams = types.SimpleNamespace()
                self.hparams = hparams
            for name in names:
                if name in locals_:
                    setattr(hparams, name, locals_[name])

        klass.save_hyperparameters = save_hyperparameters

    model = klass(**model_cfg)

    task_cfg = config.get("task", {})
    max_chunk = task_cfg.get("max_speakers_per_chunk", 4)
    max_frame = task_cfg.get("max_speakers_per_frame", 2)
    duration = task_cfg.get("duration", 10.0)
    model.specifications = Specifications(
        problem=Problem.MULTI_LABEL_CLASSIFICATION
        if max_frame is None
        else Problem.MONO_LABEL_CLASSIFICATION,
        resolution=Resolution.FRAME,
        duration=duration,
        min_duration=duration,
        warm_up=0.0,
        classes=[f"speaker#{i + 1}" for i in range(max_chunk)],
        powerset_max_classes=max_frame,
        permutation_invariant=True,
    )
    if model.specifications.powerset:
        model.powerset = Powerset(max_chunk, max_frame)
    if hasattr(model, "build"):
        model.build()

    checkpoint = torch.load(
        str(seg_dir / "pytorch_model.bin"), map_location="cpu", weights_only=False
    )
    state = checkpoint.get("state_dict", checkpoint) if isinstance(checkpoint, dict) else checkpoint
    model.load_state_dict(state, strict=True)
    model.eval()
    return model


def diarize_pyannote(samples, sample_rate, segmentation, embedding, tuning, num_speakers, device):
    from pyannote.audio.pipelines import SpeakerDiarization

    class PyannoteSpeakerDiarization(SpeakerDiarization):
        # The DiariZen fork changed to_diarization() to return (feature, activations);
        # its own apply() expects a plain feature, so unwrap it here.
        def reconstruct(self, segmentations, hard_clusters, count):
            result = super().reconstruct(segmentations, hard_clusters, count)
            if isinstance(result, tuple):
                result = result[0]
            return result

    log(f"Loading pyannote segmentation: {segmentation}")
    segmentation_model = load_pyannote_segmentation(segmentation)
    batch_size = tuning.get("batch_size", BATCH_SIZE)
    pipeline = PyannoteSpeakerDiarization(
        segmentation=segmentation_model,
        embedding=embedding,
        clustering="AgglomerativeClustering",
        segmentation_step=tuning.get("segmentation_step", 0.1),
        embedding_batch_size=batch_size,
        segmentation_batch_size=batch_size,
        device=device,
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
    turns = turns_from_annotation(result)
    # Release the diarization model/buffers before Whisper is loaded.
    del pipeline, result
    gc.collect()
    if device.type == "cuda":
        torch.cuda.empty_cache()
    return turns


def run_diarization(samples, sample_rate, descriptor, diarizer_path, embedding, tuning, num_speakers, device):
    backend = descriptor["backend"]
    if backend == "diarizen":
        return diarize_diarizen(
            samples, sample_rate, diarizer_path, embedding, tuning, num_speakers
        )
    if backend == "pyannote":
        return diarize_pyannote(
            samples, sample_rate, diarizer_path, embedding, tuning, num_speakers, device
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


def load_whisper(model_dir, device, compute_type, cpu_threads, num_workers):
    if not Path(model_dir).is_dir():
        sys.exit(f"whisper model not found: {model_dir}")
    log(f"Loading whisper model: {model_dir}")
    return WhisperModel(
        str(model_dir),
        device=device,
        compute_type=compute_type,
        cpu_threads=cpu_threads,
        num_workers=num_workers,
    )


def _transcribe_clip(model, samples, sample_rate, turn, beam_size, pad):
    """Transcribe a single speaker turn, returning one entry per Whisper segment.

    Segment timestamps are offset back to absolute file time (the clip starts at
    ``turn.start - pad``). Empty segments are dropped.
    """
    total = len(samples)
    start = max(0.0, turn["start"] - pad)
    end = min(total / sample_rate, turn["end"] + pad)
    clip = samples[int(start * sample_rate) : int(end * sample_rate)]
    if len(clip) < int(0.15 * sample_rate):
        return []
    segments, _ = model.transcribe(
        clip,
        language=LANGUAGE,
        beam_size=beam_size,
        condition_on_previous_text=False,
        word_timestamps=False,
    )
    result = []
    for segment in segments:
        text = segment.text.strip()
        if text:
            result.append(
                {"start": start + float(segment.start), "end": start + float(segment.end), "text": text}
            )
    return result


def transcribe_turns(model, samples, sample_rate, turns, beam_size, workers=1, pad=CLIP_PAD_S):
    """Transcribe each speaker turn independently, one entry per Whisper segment.

    Speaker labels come from the diarization turn, so transcription timing never
    influences speaker assignment. Turns are transcribed in parallel when
    ``workers > 1`` (the model is built with matching ``num_workers``).
    """
    log(f"Transcribing ({workers} worker(s))...")
    total = len(turns)
    lo, hi = STAGE_BOUNDS["transcription"]
    progress = {"done": 0}
    lock = threading.Lock()

    def bump():
        with lock:
            progress["done"] += 1
            done = progress["done"]
        if total:
            emit_stage("transcription", lo + (hi - lo) * (done / total), "Transcribing")

    if workers > 1 and len(turns) > 1:
        with ThreadPoolExecutor(max_workers=workers) as executor:
            futures = {
                executor.submit(
                    _transcribe_clip, model, samples, sample_rate, turn, beam_size, pad
                ): index
                for index, turn in enumerate(turns)
            }
            for future in as_completed(futures):
                turns[futures[future]]["segments"] = future.result()
                bump()
    else:
        for turn in turns:
            turn["segments"] = _transcribe_clip(
                model, samples, sample_rate, turn, beam_size, pad
            )
            bump()
    return turns


def build_lines(turns, mapping, use_hours=False):
    """One line per Whisper segment, tagged with the turn's speaker."""
    lines = []
    for turn in turns:
        speaker = mapping.get(turn["label"], UNKNOWN)
        for segment in turn.get("segments", []):
            text = segment["text"].strip()
            if not text:
                continue
            lines.append(
                {
                    "time": format_timestamp(segment["start"], use_hours),
                    "start": segment["start"],
                    "end": segment["end"],
                    "speaker": speaker,
                    "text": text,
                }
            )
    lines.sort(key=lambda line: (line["start"], line["end"]))
    return lines


def format_line(line, timestamps):
    if timestamps:
        return f"[{line['time']}] {line['speaker']}: {line['text']}"
    return f"{line['speaker']}: {line['text']}"


def join_speaker_lines(lines):
    """Merge adjacent lines that share a speaker into a single line."""
    merged = []
    for line in lines:
        if merged and merged[-1]["speaker"] == line["speaker"]:
            merged[-1]["text"] = f"{merged[-1]['text']} {line['text']}".strip()
            merged[-1]["end"] = line["end"]
        else:
            merged.append(dict(line))
    return merged


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
        help="output basename (default: <audio-stem>-transcribed)",
    )
    parser.add_argument(
        "--device",
        default="auto",
        help="torch device: auto|cpu|cuda|mps|xpu (default: auto)",
    )
    parser.add_argument(
        "--compute-type",
        default="auto",
        help="ct2 compute type; 'auto' picks float16 on cuda, int8 otherwise",
    )
    parser.add_argument(
        "--progress-json",
        action="store_true",
        help="emit line-delimited JSON progress events on stdout (logs to stderr)",
    )
    parser.add_argument(
        "--auto-tune",
        action="store_true",
        help="ignore preset batch/worker tuning and derive them from hardware",
    )
    parser.add_argument(
        "--timestamps",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="include [hh:mm:ss] timestamps in the human-readable output",
    )
    parser.add_argument(
        "--join-speaker",
        action="store_true",
        help="merge adjacent lines that share a speaker into one line",
    )
    args = parser.parse_args()

    global PROGRESS_JSON
    PROGRESS_JSON = args.progress_json

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

    output_base = Path(args.output) if args.output else REPO_ROOT / f"{audio_path.stem}-transcribed"
    out_txt = output_base.with_suffix(".txt")
    out_json = output_base.with_suffix(".json")

    backends = detect_backends()
    diarization_device = resolve_device(args.device)
    transcribe_device = transcription_device(diarization_device)
    if args.compute_type == "auto":
        compute_type = "float16" if transcribe_device == "cuda" else "int8"
    else:
        compute_type = args.compute_type

    vm = psutil.virtual_memory()
    auto = auto_tuning(
        resolved["transcriber"],
        transcribe_device,
        vm.available // (1024 * 1024),
        vm.total // (1024 * 1024),
        PHYSICAL_CORES,
    )

    tuning = dict(resolved["tuning"])
    if args.auto_tune:
        tuning["batch_size"] = 0
        tuning["transcribe_workers"] = 0
        tuning["transcribe_threads"] = 0

    batch_size = tuning.get("batch_size") or auto["batch_size"]
    workers = tuning.get("transcribe_workers") or auto["transcribe_workers"]
    transcribe_threads = tuning.get("transcribe_threads") or max(
        1, PHYSICAL_CORES // max(1, workers)
    )
    tuning["batch_size"] = batch_size
    tuning["transcribe_workers"] = workers
    tuning["transcribe_threads"] = transcribe_threads

    threads = tuning.get("threads") or PHYSICAL_CORES
    torch.set_num_threads(threads)
    try:
        torch.set_num_interop_threads(1)
    except RuntimeError:
        pass
    beam_size = tuning.get("beam_size", 5)
    log(
        f"Auto tuning: batch_size={batch_size}, transcribe_workers={workers}, "
        f"transcribe_threads={transcribe_threads} "
        f"(physical_cores={PHYSICAL_CORES}, available={vm.available // (1024 * 1024)} MB)"
    )

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
    log(f"  backends:   {', '.join(backends)}")
    log(f"  diarization device:  {diarization_device}")
    log(f"  transcription device: {transcribe_device} ({compute_type})")
    log(f"Audio: {audio_path}")

    timings = {}
    run_start = time.perf_counter()

    emit_stage("loading", 0.0, "Loading models")

    start = time.perf_counter()
    samples, sample_rate = load_audio(audio_path)
    duration = len(samples) / sample_rate
    timings["load_audio_s"] = round(time.perf_counter() - start, 3)
    log(f"Duration: {format_timestamp(duration)}")

    start = time.perf_counter()
    emit_stage("diarization", STAGE_BOUNDS["diarization"][0], "Diarizing")
    creep_stop = threading.Event()
    creep = start_creep("diarization", "Diarizing", creep_stop)
    try:
        raw_turns = run_diarization(
            samples,
            sample_rate,
            resolved["diarizer"],
            diarizer_path,
            embedding,
            tuning,
            components.NUM_SPEAKERS,
            diarization_device,
        )
    finally:
        creep_stop.set()
        creep.join(timeout=1.0)
    turns = clean_turns(raw_turns)
    timings["diarization_s"] = round(time.perf_counter() - start, 3)
    speaker_map = build_speaker_map(turns)
    emit_stage("diarization", STAGE_BOUNDS["diarization"][1], "Diarized")
    log(
        f"Detected {len(speaker_map)} speaker(s), "
        f"{len(turns)} turn(s) ({len(raw_turns)} raw)"
    )

    start = time.perf_counter()
    emit_stage("transcription", STAGE_BOUNDS["transcription"][0], "Loading whisper model")
    model = load_whisper(
        whisper_dir, transcribe_device, compute_type, transcribe_threads, workers
    )
    turns = transcribe_turns(
        model, samples, sample_rate, turns, beam_size, workers=workers
    )
    timings["transcription_s"] = round(time.perf_counter() - start, 3)

    lines = build_lines(turns, speaker_map, use_hours=duration >= 3600)
    if args.join_speaker:
        lines = join_speaker_lines(lines)

    emit_stage("writing", STAGE_BOUNDS["writing"][0], "Writing output")
    txt = "".join(f"{format_line(line, args.timestamps)}\n" for line in lines)
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
            "diarization_device": str(diarization_device),
            "transcription_device": transcribe_device,
            "backends": backends,
            "compute_type": compute_type,
            "num_speakers": components.NUM_SPEAKERS,
            "beam_size": beam_size,
            "threads": threads,
            "batch_size": batch_size,
            "transcribe_workers": workers,
            "transcribe_threads": transcribe_threads,
            "physical_cores": PHYSICAL_CORES,
            "auto_tune": args.auto_tune,
            "auto": auto,
            "timestamps": args.timestamps,
            "join_speaker": args.join_speaker,
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

    emit_stage("writing", STAGE_BOUNDS["writing"][1], "Done")
    emit({"event": "done", "txt": str(out_txt), "json": str(out_json),
          "stats": {
              "audio_seconds": round(duration, 2),
              "run_seconds": round(time.perf_counter() - run_start, 2),
              "speakers": len(speaker_map),
              "turns": len(turns),
              "lines": len(lines),
              "preset": resolved["preset"],
              "diarizer": resolved["diarizer_name"],
              "embedding": resolved["embedding_name"],
              "transcriber": resolved["transcriber_name"],
              "device": str(diarization_device),
              "transcription_device": transcribe_device,
              "timings": timings,
          }})

    log("")
    for line in lines:
        log(format_line(line, args.timestamps))

    log("")
    log(f"Wrote {out_txt}")
    log(f"Wrote {out_json}")


if __name__ == "__main__":
    try:
        main()
    except SystemExit as exc:
        if exc.code not in (0, None):
            emit({"event": "error", "message": str(exc.code)})
        raise
    except Exception as exc:  # noqa: BLE001
        emit({"event": "error", "message": str(exc)})
        raise
