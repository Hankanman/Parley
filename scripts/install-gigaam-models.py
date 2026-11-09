#!/usr/bin/env python3
"""Install already-exported bundles, preserving pre-Parley user-data migration."""
import argparse
import json
import os
from pathlib import Path
import shutil
import tempfile

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--bundles", type=Path, required=True)
p.add_argument("--output", type=Path)
a = p.parse_args()
base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share"))
current = base / "io.github.hankanman.Parley"
legacy = base / "com.meetily.ai"
# Never create a new partial Parley data directory beside the old database:
# that would bypass the application's one-time legacy migration.
app_data = legacy if not current.exists() and legacy.is_dir() else current
output = a.output or app_data / "models/gigaam"
output.mkdir(parents=True, exist_ok=True)
for name in ("v3_ctc", "v3_rnnt", "v3_e2e_ctc", "v3_e2e_rnnt"):
    source = a.bundles / name
    manifest = json.loads((source / "manifest.json").read_text())
    if manifest["name"] != name:
        raise SystemExit(f"Wrong bundle: {source}")
    destination = output / name
    if destination.exists():
        if (destination / "manifest.json").read_bytes() == (source / "manifest.json").read_bytes():
            print(f"Already installed: {name}")
            continue
        raise SystemExit(f"Refusing to overwrite existing bundle: {destination}")
    staging = Path(tempfile.mkdtemp(prefix=f".{name}-", dir=output))
    try:
        shutil.copytree(source, staging, dirs_exist_ok=True)
        os.rename(staging, destination)
    finally:
        if staging.exists():
            shutil.rmtree(staging)
    print(f"Installed: {destination}")
