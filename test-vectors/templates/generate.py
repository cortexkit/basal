"""Reproduce the public instance ids independently of basal's Rust implementation.

Requires Python 3 and the blake3 package. Inputs show valid caller values;
UTC timestamps are normalized below and paths are already resolved.
"""
import json
import struct
from datetime import datetime
from pathlib import Path

import blake3

INPUTS = [
    ("watch", "agent_47120287c700722b", {}),
    ("watch", "agent_47120287c700722b", {"path": "/work/notes/todo", "interval": "15m"}),
    ("watch", "agent_d1e7002e2c9b9f43", {"path": "/work/notes/todo", "interval": "15m"}),
    ("reminder", "agent_47120287c700722b", {"at": "2026-11-01T09:00:00Z", "note": 'Grüße, 東京: "watch" \\ here', "count": -2}),
    ("watch", "agent_47120287c700722b", {"path": "/work/notes/todo", "interval": "900s"}),
    ("reminder", "agent_47120287c700722b", {"at": "2026-11-01T09:00:00Z"}),
    ("reminder", "agent_47120287c700722b", {"at": "2026-11-01T09:00:00.000+00:00"}),
    ("reminder", "agent_47120287c700722b", {"at": "2026-11-01T09:00:00.1Z"}),
]

vectors = []
for template, agent, params in INPUTS:
    supplied = params
    params = dict(params)
    if "at" in params:
        at = datetime.fromisoformat(params["at"])
        millis = at.microsecond // 1000
        params["at"] = at.strftime("%Y-%m-%dT%H:%M:%S") + (f".{millis:03}" if millis else "") + "Z"
    canonical = json.dumps(params, sort_keys=True, ensure_ascii=False, separators=(",", ":"))
    framed = b"basal-template-instance-v1\0"
    for text in (template, agent, canonical):
        encoded = text.encode("utf-8")
        framed += struct.pack("<Q", len(encoded)) + encoded
    digest = blake3.blake3(framed).hexdigest()
    vectors.append({"template": template, "agent_id": agent, "params": params, "supplied_params": supplied,
                    "canonical_params": canonical, "hash": digest,
                    "flow_id": template + "_" + digest[:16]})
Path(__file__).with_name("instance-ids.json").write_text(
    json.dumps(vectors, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(f"Generated {len(vectors)} independently framed BLAKE3 instance id vectors")
