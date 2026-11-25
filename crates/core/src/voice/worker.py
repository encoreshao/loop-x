import json
import sys

import numpy as np
from faster_whisper import WhisperModel

models = {}
for line in sys.stdin:
    try:
        req = json.loads(line)
        name = req["model"]
        if name not in models:
            models.clear()
            models[name] = WhisperModel(name, device="cpu", compute_type="int8", local_files_only=True)
        audio = np.fromfile(req["audio"], dtype=np.float32)
        segments, info = models[name].transcribe(
            audio, language=req.get("language") or None, beam_size=5, vad_filter=True
        )
        out = {"text": "".join(s.text for s in segments).strip(), "language": info.language}
    except Exception as e:
        out = {"error": f"{type(e).__name__}: {e}"}
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()
