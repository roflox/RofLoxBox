"""RofLoxBox's Demucs helper: loads a model once, then separates the pieces
it is given, so live stems don't wait for Python and the model each time.

Run with the Python that Demucs is installed in:
    python demucs_worker.py <model name>
Then, one JSON object per line on stdin:
    {"input": "piece.wav", "output": "folder"}
and one per line back on stdout once each is done:
    {"done": true} or {"error": "..."}
The first line it prints is {"ready": [stem names]} once the model is loaded
(or {"error": "..."}). The stems are written as float WAV files named after
them, at their real level (no rescaling), like
`demucs --float32 --clip-mode none`.
"""

import json
import os
import sys
import traceback


def reply(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def device():
    import torch

    if torch.cuda.is_available():
        return "cuda"
    if getattr(torch.backends, "mps", None) and torch.backends.mps.is_available():
        return "mps"
    return "cpu"


def main():
    try:
        import soundfile
        import torch
        from demucs.apply import apply_model
        from demucs.audio import convert_audio
        from demucs.pretrained import get_model

        # Separating is background work: loading tracks and the app's own
        # work come first.
        try:
            os.nice(10)
        except (AttributeError, OSError):
            pass
        torch.set_num_threads(max(1, (os.cpu_count() or 2) // 2))
        model = get_model(sys.argv[1])
        model.eval()
        where = device()
        try:
            # A first run on silence: the GPU readies its code now rather
            # than on the first piece.
            silence = torch.zeros(1, model.audio_channels, model.samplerate)
            with torch.no_grad():
                apply_model(model, silence, device=where, progress=False)
        except Exception:
            where = "cpu"
        reply({"ready": list(model.sources), "device": where})
    except Exception as error:
        reply({"error": f"{type(error).__name__}: {error}"})
        return

    for line in sys.stdin:
        try:
            job = json.loads(line)
            data, rate = soundfile.read(job["input"], dtype="float32", always_2d=True)
            wav = convert_audio(torch.from_numpy(data.T.copy()), rate, model.samplerate, model.audio_channels)
            ref = wav.mean(0)
            mean, std = ref.mean(), ref.std() + 1e-8
            with torch.no_grad():
                out = apply_model(model, ((wav - mean) / std)[None], device=where, progress=False)
            out = out[0] * std + mean
            os.makedirs(job["output"], exist_ok=True)
            for name, stem in zip(model.sources, out):
                path = os.path.join(job["output"], name + ".wav")
                soundfile.write(path, stem.cpu().numpy().T, model.samplerate, subtype="FLOAT")
            reply({"done": True})
        except Exception as error:
            reply({"error": f"{type(error).__name__}: {error}", "trace": traceback.format_exc()})


if __name__ == "__main__":
    main()
