#!/usr/bin/env bash
# Copyright 2026 Blue2th
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# Compile java/A2dpServiceListener.java to a standalone classes.dex and install it
# at assets/a2dp_listener.dex (embedded into the Rust binary via include_bytes!).
#
# Tool paths default to this machine's Android Studio JBR + SDK, but each can be
# overridden via environment variables.

set -euo pipefail

JAVAC="${JAVAC:-/home/orel/.local/android-studio/jbr/bin/javac}"
D8="${D8:-/home/orel/Android/Sdk/build-tools/37.0.0/d8}"
ANDROID_JAR="${ANDROID_JAR:-/home/orel/Android/Sdk/platforms/android-34/android.jar}"

# Resolve repository paths relative to this script so it can be run from anywhere.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SRC="${SCRIPT_DIR}/A2dpServiceListener.java"
ASSET_DIR="${REPO_ROOT}/assets"
ASSET_DEX="${ASSET_DIR}/a2dp_listener.dex"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

# 1. Compile the Java source against the Android API stubs, targeting Java 8
#    bytecode (a level d8 accepts).
"${JAVAC}" -cp "${ANDROID_JAR}" -d "${TMP_DIR}" --release 8 "${SRC}"

# 2. Dex the compiled class.
"${D8}" --lib "${ANDROID_JAR}" --output "${TMP_DIR}" \
    "${TMP_DIR}/dev/dioxus/main/A2dpServiceListener.class"

# 3. Install the resulting classes.dex as the committed embedded asset.
mkdir -p "${ASSET_DIR}"
mv "${TMP_DIR}/classes.dex" "${ASSET_DEX}"

echo "Built ${ASSET_DEX} ($(wc -c < "${ASSET_DEX}") bytes)"
