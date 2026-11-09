#!/usr/bin/env python3
"""Stage the official native ONNX Runtime and CUDA libraries from a build-only venv."""
import argparse
from pathlib import Path
import shutil
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("site_packages", type=Path)
p.add_argument("output", type=Path)
p.add_argument("--onnxruntime", type=Path, required=True)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
capi = a.onnxruntime / "lib"
ort = capi / "libonnxruntime.so.1.24.4"
if not ort.exists():
    raise SystemExit("Expected official ONNX Runtime GPU CUDA 13 version 1.24.4")
existing = a.output / "libonnxruntime.so"
if existing.exists() and existing.resolve().name != ort.name:
    raise SystemExit("Use an empty staging directory when changing ONNX Runtime versions")
files = list(capi.glob("*.so*")) + list((a.site_packages / "nvidia").rglob("*.so*"))
for file in files:
    if "pybind" in file.name or "tensorrt" in file.name or file.is_dir() or file.is_symlink():
        continue
    if file.name.startswith(("libcuda.so", "libnvidia-")):
        raise SystemExit("Driver libraries must not be bundled")
    dest = a.output / file.name
    if not dest.exists() or dest.stat().st_size != file.stat().st_size:
        shutil.copy2(file, dest)
    subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", str(dest)], check=True)
    soname = subprocess.check_output(["patchelf", "--print-soname", str(dest)], text=True).strip()
    if soname and soname != dest.name:
        alias = a.output / soname
        if not alias.exists():
            alias.symlink_to(dest.name)
link = a.output / "libonnxruntime.so"
if link.is_symlink():
    link.unlink()
link.symlink_to(ort.name)
licenses = a.output / "licenses"
licenses.mkdir(exist_ok=True)
for distribution in a.site_packages.glob("*.dist-info"):
    if distribution.name.startswith("nvidia_"):
        source = distribution / "licenses"
        if source.is_dir():
            shutil.copytree(source, licenses / distribution.name, dirs_exist_ok=True)
for name in ("LICENSE", "ThirdPartyNotices.txt"):
    source = a.onnxruntime / name
    if source.exists():
        shutil.copy2(source, licenses / ("onnxruntime-" + name))
print(f"Staged native GigaAM runtime in {a.output}")
