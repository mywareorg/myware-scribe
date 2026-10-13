#!/usr/bin/env python3
"""Create the single transiel virtual environment.

DiariZen pins old CPU torch (2.1.1) and bundles its own pyannote.audio fork, so
it cannot share an environment with a modern standalone pyannote.audio. That is
why transiel uses one dedicated Python 3.11 environment for everything.

Usage:
    python scripts/bootstrap.py

Requires a Python 3.11 interpreter on PATH (e.g. "python3.11" on POSIX or
"py -3.11" on Windows). Override with the PYTHON environment variable.
"""

import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
VENV = REPO_ROOT / ".venv"
CLONE = REPO_ROOT / "third_party" / "DiariZen"

TORCH_INDEX = "https://download.pytorch.org/whl/cpu"
TORCH_PINS = ("torch==", "torchaudio==")


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
    if not CLONE.is_dir():
        sys.exit(f"DiariZen source not found: {CLONE}")

    python = find_python()

    if not venv_python().exists():
        print(f"Creating virtual environment at {VENV}")
        run(python + ["-m", "venv", str(VENV)])
    else:
        print(f"Reusing virtual environment at {VENV}")

    py = str(venv_python())
    run([py, "-m", "pip", "install", "--upgrade", "pip"])

    torch_reqs, other_reqs = split_requirements(read_requirements())

    print("Installing CPU torch")
    run([py, "-m", "pip", "install", *torch_reqs, "--index-url", TORCH_INDEX])

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

    print(f"\nEnvironment ready: {VENV}")
    print(f"Interpreter: {py}")


if __name__ == "__main__":
    main()
