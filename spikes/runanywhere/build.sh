#!/bin/bash
# SPIKE (spike/runanywhere, never merged): build RunAnywhere's Python binding from source and run spike.py.
# Evaluation only — see research/RUNANYWHERE-SPIKE.md in the sidevoice workspace.
#
# RunAnywhere is not on PyPI (pypi.org/pypi/runanywhere → 404 on 2026-10-01). Pinned:
# RunanywhereAI/runanywhere-sdks v0.20.37 = a18d1d36a3f2b1935e4de83194d0e83dfe669a2c.
#
# Written for the dev pod: no system compiler, cmake, ar, bzip2 or libc headers, so zig is the C/C++ compiler and
# the build tools come from pip. On a normal machine drop the wrappers and the four pod-only defines marked below.
set -euo pipefail
W=${W:-/tmp/ra-spike}
REF=a18d1d36a3f2b1935e4de83194d0e83dfe669a2c
mkdir -p "$W/bin" "$W/wheels"

# Toolchain: cmake/ninja/scikit-build-core/pybind11 from pip, protoc + grpcio-tools for their IDL codegen.
uv venv -q "$W/pyenv" --python 3.12
uv pip install -q --python "$W/pyenv/bin/python" pip cmake ninja scikit-build-core pybind11 build numpy \
  grpcio-tools "protobuf>=5.29,<7"
[ -d "$W/ra" ] || git clone -q https://github.com/RunanywhereAI/runanywhere-sdks.git "$W/ra"
git -C "$W/ra" checkout -q "$REF"
PROTOC=$(grep '^PROTOC_VERSION=' "$W/ra/core/VERSIONS" | cut -d= -f2)
if [ ! -x "$W/protoc/bin/protoc" ]; then
  curl -sSL -o "$W/protoc.zip" "https://github.com/protocolbuffers/protobuf/releases/download/v$PROTOC/protoc-$PROTOC-linux-x86_64.zip"
  "$W/pyenv/bin/python" -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$W/protoc.zip" "$W/protoc"
  chmod +x "$W/protoc/bin/protoc"
fi
ln -sf "$W/protoc/bin/protoc" "$W/bin/protoc"

# Pod-only wrappers: zig as cc/c++, zig ar/ranlib, a bzip2 decompressor for tar -j.
for t in cc:cc cxx:c++; do
  printf '#!/bin/sh\nfor a in "$@"; do case "$a" in --target=*|-Wl,--no-undefined-version) ;; *) set -- "$@" "$a" ;; esac; shift; done\nexec zig %s -target x86_64-linux-gnu.2.36 "$@"\n' "${t#*:}" > "$W/bin/zig${t%%:*}"
done
printf '#!/bin/sh\nexec zig ar "$@"\n' > "$W/bin/ar"
printf '#!/bin/sh\nexec zig ranlib "$@"\n' > "$W/bin/ranlib"
printf '#!/bin/sh\nexec %s -c "import bz2,sys,shutil; shutil.copyfileobj(bz2.BZ2File(sys.stdin.buffer), sys.stdout.buffer, 1<<20)"\n' "$W/pyenv/bin/python" > "$W/bin/bzip2"
chmod +x "$W/bin/"*
export PATH="$W/pyenv/bin:$W/bin:$PATH"

# Their sherpa-onnx fork prebuilt (RunanywhereAI/sherpa-onnx 1.13.5, ORT 1.28.0). Its Linux x64 asset needs
# glibc >= 2.38 / GLIBCXX_3.4.32; the pod has 2.36, so the libsherpa-onnx-c-api.so from their own v0.20.37
# cpp-desktop kit (glibc 2.34) is swapped in.
(cd "$W/ra" && bash core/scripts/linux/download-sherpa-onnx.sh)
if [ ! -f "$W/kit/cpp-desktop-linux-x64/third_party/libsherpa-onnx-c-api.so" ]; then
  mkdir -p "$W/kit"
  gh release download v0.20.37 -R RunanywhereAI/runanywhere-sdks -D "$W/kit" --clobber \
    -p 'RunAnywhere-cpp-desktop-linux-x64-v0.20.37.tar.gz*'
  (cd "$W/kit" && sha256sum -c RunAnywhere-cpp-desktop-linux-x64-v0.20.37.tar.gz.sha256 && tar xzf RunAnywhere-cpp-desktop-linux-x64-v0.20.37.tar.gz)
fi
cp "$W/kit/cpp-desktop-linux-x64/third_party/libsherpa-onnx-"*-api.so "$W/ra/core/third_party/sherpa-onnx-linux/lib/"

# The wheel: speech only (llama.cpp off), CPU only.
cd "$W/ra/bindings/python"
CC="$W/bin/zigcc" CXX="$W/bin/zigcxx" CMAKE_GENERATOR=Ninja CMAKE_BUILD_PARALLEL_LEVEL=6 \
pip wheel . --no-build-isolation --no-deps -w "$W/wheels" \
  -C build-dir="$W/pybuild" \
  -C cmake.define.RAC_BACKEND_LLAMACPP=OFF \
  -C cmake.define.RAC_GPU_VULKAN=OFF \
  -C cmake.define.POSIX_REGEX_LIB=NONE `# pod-only: libarchive finds no libc regex.h without system headers` \
  -C cmake.define.CMAKE_AR="$W/bin/ar" `# pod-only` \
  -C cmake.define.CMAKE_RANLIB="$W/bin/ranlib" `# pod-only` \
  -C cmake.define.CMAKE_CXX_SCAN_FOR_MODULES=OFF `# pod-only: CMake 4 + clang wants clang-scan-deps`
# Note: a configure that fails once leaves BZIP2_FOUND cached and the bundled bzip2 target undefined on the next
# run ("libbz2_bundled.a missing and no known rule"): delete the build dir before retrying.

uv venv -q "$W/pyrun" --python 3.12
uv pip install -q --python "$W/pyrun/bin/python" "$W"/wheels/runanywhere-0.20.37-*.whl
# The wheel is not auditwheel-repaired: the sherpa-onnx and ONNX Runtime libraries it links come from outside.
cd "$(dirname "$0")"
LD_LIBRARY_PATH="$W/ra/core/third_party/sherpa-onnx-linux/lib" RUNANYWHERE_HOME="$W/rahome" \
  "$W/pyrun/bin/python" spike.py
