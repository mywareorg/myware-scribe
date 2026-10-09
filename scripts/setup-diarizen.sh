#!/usr/bin/env bash
# Create the dedicated DiariZen environment used by transiel's default backend.
#
# DiariZen pins an old torch and bundles its own pyannote.audio fork, so it
# cannot share transiel's main virtualenv. Python 3.11 is required because
# the pinned torch==2.1.1 wheels are not available for 3.12.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENV="${REPO_ROOT}/.venv-diarizen"
CLONE="${REPO_ROOT}/third_party/DiariZen"
PYTHON="${PYTHON:-python3.11}"

if [ ! -d "${CLONE}" ]; then
    echo "Cloning DiariZen into ${CLONE}"
    git clone --depth 1 https://github.com/BUTSpeechFIT/DiariZen "${CLONE}"
fi

if [ ! -d "${VENV}" ]; then
    echo "Creating virtualenv at ${VENV}"
    "${PYTHON}" -m venv "${VENV}"
fi

PIP="${VENV}/bin/pip"
"${PIP}" install --upgrade pip

echo "Installing torch (CPU build)"
"${PIP}" install torch==2.1.1 torchaudio==2.1.1 \
    --index-url https://download.pytorch.org/whl/cpu

echo "Installing DiariZen requirements"
"${PIP}" install einops h5py joblib jupyterlab tensorboard librosa matplotlib \
    "numpy==1.26.4" openpyxl pandas pesq pystoi pyyaml scipy soundfile tabulate \
    toml torchinfo tqdm "accelerate==1.6.0" thop

echo "Installing DiariZen"
"${PIP}" install -e "${CLONE}"

echo "Installing bundled pyannote.audio fork"
"${PIP}" install -e "${CLONE}/pyannote-audio" -c "${CLONE}/constraints.txt"

echo "DiariZen environment ready at ${VENV}"
