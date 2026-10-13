#!/usr/bin/env python3
"""Create the single transiel virtual environment.

DiariZen pins old torch (2.1.1) and bundles its own pyannote.audio fork, so it
cannot share an environment with a modern standalone pyannote.audio. That is why
transiel uses one dedicated Python 3.11 environment for everything.

The torch wheel channel is vendor-selectable so the pipeline can run on CPU,
NVIDIA (CUDA), AMD (ROCm) or Apple (MPS) builds:

    python scripts/bootstrap.py --vendor auto|cpu|cuda|rocm|mps

`auto` detects the platform (macOS -> mps, nvidia-smi -> cuda, /opt/rocm ->
rocm, else cpu). Set the TORCH_INDEX environment variable to override the wheel
index explicitly. Requires a Python 3.11 interpreter on PATH (e.g.
"python3.11" on POSIX or "py -3.11" on Windows); override with PYTHON.
"""

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
VENV = REPO_ROOT / ".venv"
CLONE = REPO_ROOT / "third_party" / "DiariZen"

TORCH_PINS = ("torch==", "torchaudio==")

# Vendor -> PyTorch wheel index. MPS ships in the default PyPI wheels, so no
# index override is needed there.
TORCH_CHANNELS = {
    "cpu": "https://download.pytorch.org/whl/cpu",
    "cuda": "https://download.pytorch.org/whl/cu121",
    "rocm": "https://download.pytorch.org/whl/rocm5.7",
    "mps": None,
}


def detect_vendor():
    if sys.platform == "darwin":
        return "mps"
    if shutil.which("nvidia-smi"):
        return "cuda"
    if Path("/opt/rocm").exists():
        return "rocm"
    return "cpu"


def torch_index(vendor):
    override = os.environ.get("TORCH_INDEX")
    if override:
        return override
    return TORCH_CHANNELS.get(vendor)



def read_requirements():
    lines = (REPO_ROOT / "requirements.txt").read_text(encoding="utf-8").splitlines()
    return [line.strip() for line in lines if line.strip() and not line.startswith("#")]


def split_requirements(requirements):
    torch_reqs = [r for r in requirements if r.startswith(TORCH_PINS)]
    other_reqs = [r for r in requirements if not r.startswith(TORCH_PINS)]
    return torch_reqs, other_reqs


def find_python():
    override = os.environ.get("PYTHON")
    if override:
        return [override]
    if os.name == "nt":
        return ["py", "-3.11"]
    return ["python3.11"]


def run(cmd, **kwargs):
    print("+", " ".join(str(c) for c in cmd), flush=True)
    subprocess.run([str(c) for c in cmd], check=True, **kwargs)


def venv_python():
    if os.name == "nt":
        return VENV / "Scripts" / "python.exe"
    return VENV / "bin" / "python"


def main():
    parser = argparse.ArgumentParser(description="Create the transiel environment")
    parser.add_argument(
        "--vendor",
        choices=["auto", "cpu", "cuda", "rocm", "mps"],
        default="auto",
        help="torch wheel channel (default: auto-detect)",
    )
    args = parser.parse_args()

    if not CLONE.is_dir():
        sys.exit(f"DiariZen source not found: {CLONE}")

    vendor = detect_vendor() if args.vendor == "auto" else args.vendor
    index = torch_index(vendor)

    python = find_python()

    if not venv_python().exists():
        print(f"Creating virtual environment at {VENV}")
        run(python + ["-m", "venv", str(VENV)])
    else:
        print(f"Reusing virtual environment at {VENV}")

    py = str(venv_python())
    run([py, "-m", "pip", "install", "--upgrade", "pip"])

    torch_reqs, other_reqs = split_requirements(read_requirements())

    print(f"Installing torch ({vendor})")
    torch_cmd = [py, "-m", "pip", "install", *torch_reqs]
    if index:
        torch_cmd += ["--index-url", index]
    run(torch_cmd)

    print("Installing runtime dependencies")
    run([py, "-m", "pip", "install", *other_reqs])

    print("Installing DiariZen")
    run([py, "-m", "pip", "install", "-e", str(CLONE)])

    print("Installing bundled pyannote.audio fork")
    run(
        [
            py,
            "-m",
            "pip",
            "install",
            "-e",
            str(CLONE / "pyannote-audio"),
            "-c",
            str(CLONE / "constraints.txt"),
        ]
    )

    if vendor == "cuda":
        print(
            "\nNote: GPU transcription (faster-whisper) additionally needs a "
            "matching CUDA/cuDNN runtime available to CTranslate2."
        )

    print(f"\nEnvironment ready: {VENV}")
    print(f"Interpreter: {py}")
    print(f"Torch channel: {vendor}" + (f" ({index})" if index else ""))


if __name__ == "__main__":
    main()
