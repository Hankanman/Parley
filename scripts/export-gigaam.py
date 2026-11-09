#!/usr/bin/env python3
"""Prepare native GigaAM bundles. Python is only used by this offline build tool.

Run with the pinned GigaAM checkout installed (see docs/gigaam.md). Models are
published atomically; a failed export never makes a partial bundle selectable.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request

MODELS = ["v3_e2e_rnnt", "v3_e2e_ctc", "v3_rnnt", "v3_ctc"]
UPSTREAM_REVISION = "7447938d791c4f3e643386ee22c33777004293a5"


def export(name, output, cache, sample):
    import gigaam
    import numpy as np
    import torch
    from gigaam.preprocess import load_audio

    destination = output / name
    if (destination / "manifest.json").exists():
        shutil.copyfile(Path(__file__).with_name("gigaam-LICENSE"), destination / "LICENSE")
        print(f"Already prepared: {name}", flush=True)
        return
    model = gigaam.load_model(name, device="cpu", fp16_encoder=False, download_root=str(cache))
    stage = Path(tempfile.mkdtemp(prefix=f".{name}-", dir=output))
    try:
        model.to_onnx(str(stage), dtype=torch.float32)
        mel = model.preprocessor.featurizer[0]
        tokenizer = model.decoding.tokenizer
        vocab = [tokenizer.id_to_str(i) for i in range(len(tokenizer))]
        tokenizer_file = None
        if not tokenizer.charwise:
            tokenizer_file = "tokenizer.model"
            (stage / tokenizer_file).write_bytes(tokenizer.model.serialized_model_proto())
        n_fft = mel.n_fft
        window = mel.spectrogram.window
        if window.numel() != n_fft:
            padding = (n_fft - window.numel()) // 2
            window = torch.nn.functional.pad(window, (padding, n_fft - window.numel() - padding))
        rnnt = "rnnt" in name
        manifest = {
            "version": 1, "name": name, "upstream_revision": UPSTREAM_REVISION,
            "encoder": f"{name}_encoder.onnx" if rnnt else f"{name}.onnx",
            "decoder": f"{name}_decoder.onnx" if rnnt else None,
            "joint": f"{name}_joint.onnx" if rnnt else None,
            "tokenizer": tokenizer_file, "vocab": vocab,
            "hidden": int(model.head.decoder.pred_hidden) if rnnt else 0,
            "layers": int(model.head.decoder.lstm.num_layers) if rnnt else 0,
            "max_symbols": int(model.decoding.max_symbols) if rnnt else 1,
            "n_fft": n_fft, "hop_length": mel.hop_length, "center": mel.spectrogram.center,
            "window": window.tolist(), "mel_filters": mel.mel_scale.fb.tolist(),
        }
        # A reproducible public Russian sample, plus its official text/features.
        pcm = load_audio(str(sample))
        if pcm.numel() > 24 * 16000:
            pcm = pcm[:24 * 16000]
        with torch.inference_mode():
            encoded, lengths = model(pcm.unsqueeze(0), torch.tensor([pcm.numel()]))
            text, _ = model._decode(encoded, lengths, torch.tensor([pcm.numel()]), False)[0]
            features, _ = model.preprocessor(pcm.unsqueeze(0), torch.tensor([pcm.numel()]))
        pcm.numpy().astype("<f4").tofile(stage / "reference.pcm")
        features.numpy().astype("<f4").tofile(stage / "reference.features")
        (stage / "reference.json").write_text(json.dumps({"text": text, "samples": pcm.numel(), "feature_shape": list(features.shape)}, ensure_ascii=False))
        manifest["files"] = {}
        for file in stage.iterdir():
            if file.suffix in {".onnx", ".data", ".model"} or ".onnx." in file.name:
                digest = hashlib.file_digest(file.open("rb"), "sha256").hexdigest()
                manifest["files"][file.name] = {"bytes": file.stat().st_size, "sha256": digest}
        (stage / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False))
        shutil.copyfile(Path(__file__).with_name("gigaam-LICENSE"), stage / "LICENSE")
        os.rename(stage, destination)
        print(f"Prepared {name}: {text}", flush=True)
    finally:
        if stage.exists():
            shutil.rmtree(stage)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cache", type=Path, default=Path.home() / ".cache/gigaam")
    parser.add_argument("--models", nargs="+", choices=MODELS, default=MODELS)
    parser.add_argument("--sample", type=Path)
    args = parser.parse_args()
    import torch
    torch.set_num_threads(8)
    args.output.mkdir(parents=True, exist_ok=True)
    args.cache.mkdir(parents=True, exist_ok=True)
    sample = args.sample or args.cache / "example.wav"
    if not sample.exists():
        urllib.request.urlretrieve("https://cdn.chatwm.opensmodel.sberdevices.ru/GigaAM/example.wav", sample)
    for name in args.models:
        export(name, args.output, args.cache, sample)


if __name__ == "__main__":
    main()
