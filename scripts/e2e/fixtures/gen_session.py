#!/usr/bin/env python3
"""Synthetic session-log generator + real-corpus sampler for rness E2E/perf.

Writes `<root>/<ULID>/session.v1.jsonl` files in the durable v1 format
(crates/rness-protocol/src/events.rs). Standard library only.

Generator (module):
    from fixtures.gen_session import generate, sample_real
    ids = generate(root, workspace="/path/cwd", turns=50, tools_per_turn=2,
                   tool_result_bytes=4096, compactions=2, prunes=3, forks=1,
                   attempts=1, sessions=1, seed=7)
    # -> list of session ids (roots first, then forks)

    copied = sample_real(dest_root, n=5)   # COPIES the n largest real sessions
                                           # (+ fork ancestors) from ~/.rness/sessions

CLI:
    python3 scripts/e2e/fixtures/gen_session.py gen  ROOT --workspace DIR [--turns 50 ...]
    python3 scripts/e2e/fixtures/gen_session.py real ROOT [--n 5] [--source ~/.rness/sessions] [--workspace DIR]

Structure produced per turn (matches what the engine writes):
    user/message, turn/started, [assistant/attempt (error)]*,
    (assistant/message tool_use, tool/result)*tools_per_turn, assistant/message end_turn, turn/ended
Compaction k (spread evenly) at a turn boundary:
    compaction/started{sources} -> compaction/summary{replaces} -> compaction/finished{started}
    `replaces` = every model-visible event since the previous checkpoint
    (user/assistant/tool events + the previous checkpoint id + live prunes),
    i.e. the non-expanded "direct fold" form current writers use.
Prune: compaction/prune{replaces: <tool/result id>, result: <same result, middle elided>}.
Fork: a new session whose header has parent{session, at=<parent event id>} plus its own turns.
"""
import argparse
import json
import os
import random
import shutil
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


class UlidClock:
    """Monotonic ULIDs: strictly increasing ms timestamps from `start_ms`."""

    def __init__(self, start_ms=None, rng=None, step_ms=7):
        self.ms = start_ms or int(time.time() * 1000) - 86_400_000
        self.rng = rng or random.Random()
        self.step = step_ms

    def next(self):
        self.ms += self.step
        return self.ulid(self.ms), self.stamp(self.ms)

    def ulid(self, ms):
        value = (ms << 80) | self.rng.getrandbits(80)
        return "".join(CROCKFORD[(value >> (5 * i)) & 31] for i in reversed(range(26)))

    @staticmethod
    def stamp(ms):
        return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}Z"


WORDS = ("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau "
         "upsilon phi chi psi omega refactor kernel session engine provider stream compaction tool result").split()


def filler(rng, nbytes):
    if nbytes <= 0:
        return ""
    words = []
    size = 0
    while size < nbytes:
        w = rng.choice(WORDS)
        words.append(w)
        size += len(w) + 1
        if rng.random() < 0.08:
            words.append("\n")
    return " ".join(words)[:nbytes]


class Writer:
    def __init__(self, root, session, clock):
        self.dir = Path(root) / session
        self.dir.mkdir(parents=True, exist_ok=True)
        self.f = open(self.dir / "session.v1.jsonl", "w")
        self.clock = clock
        self.ids = []

    def emit(self, event):
        eid, at = self.clock.next()
        self.f.write(json.dumps({"id": eid, "at": at, **event}, ensure_ascii=False, separators=(",", ":")) + "\n")
        self.ids.append(eid)
        return eid

    def close(self):
        self.f.close()


def _write_turns(w, rng, turn0, turns, tools_per_turn, tool_result_bytes, attempts, model, visible,
                 compact_at, prunes_left, text_bytes):
    """Append `turns` turns. `visible` collects model-visible event ids since the
    last checkpoint; compactions fire before turns listed in `compact_at`."""
    tool_results = []
    for t in range(turns):
        turn = turn0 + t
        if turn in compact_at and visible:
            sources = list(visible)
            started = w.emit({"type": "compaction/started", "model": model, "sources": sources,
                              "estimated_input": 1000 + 50 * len(sources)})
            summary = w.emit({"type": "compaction/summary", "replaces": sources, "model": model,
                              "summary": f"## Primary Request and Intent\n- synthetic checkpoint before turn {turn}\n\n"
                                         + filler(rng, 600)})
            w.emit({"type": "compaction/finished", "started": started, "outcome": "committed",
                    "usage": {"input_tokens": 1000, "output_tokens": 300}, "chunks": []})
            visible.clear()
            visible.append(summary)
            tool_results.clear()  # prunes only target results still visible
        visible.append(w.emit({"type": "user/message", "intent": "followup",
                               "content": [{"kind": "text", "text": f"turn {turn}: " + filler(rng, text_bytes)}]}))
        w.emit({"type": "turn/started", "turn": turn})
        for a in range(attempts):
            w.emit({"type": "assistant/attempt", "model": model, "chunks": [], "outcome": {
                "kind": "error", "message": "provider returned 529: overloaded", "retryable": True,
                "code": "PROVIDER", "retry_in_ms": 500 * (2 ** a)}})
        for k in range(tools_per_turn):
            call = f"toolu_gen_{turn}_{k}_{rng.getrandbits(32):08x}"
            command = f"echo synthetic {turn}.{k}"
            visible.append(w.emit({"type": "assistant/message", "model": model,
                                   "content": [{"kind": "text", "text": filler(rng, 80)},
                                               {"kind": "tool_use", "call": call, "name": "Bash",
                                                "args": {"command": command, "description": "synthetic"}}],
                                   "stop": "tool_use", "usage": {"input_tokens": 1000 + turn, "output_tokens": 40},
                                   "chunks": []}))
            output = filler(rng, tool_result_bytes)
            result = {"call": call, "name": "Bash", "content": [{"kind": "text", "text": output}], "output": output,
                      "is_error": False, "duration_ms": rng.randint(5, 900)}
            rid = w.emit({"type": "tool/result", **result})
            visible.append(rid)
            tool_results.append((rid, result))
        if prunes_left[0] > 0 and tool_results:
            rid, result = tool_results.pop(0)
            out = result["output"]
            keep = max(16, len(out) // 10)
            short = out[:keep] + f"\n[... {len(out) - 2 * keep} bytes pruned ...]\n" + out[-keep:]
            pruned = dict(result, output=short, content=[{"kind": "text", "text": short}])
            pid = w.emit({"type": "compaction/prune", "replaces": rid, "result": pruned})
            visible.remove(rid)
            visible.append(pid)
            prunes_left[0] -= 1
        visible.append(w.emit({"type": "assistant/message", "model": model,
                               "content": [{"kind": "text", "text": f"done with turn {turn}. " + filler(rng, text_bytes)}],
                               "stop": "end_turn", "usage": {"input_tokens": 1200 + turn, "output_tokens": 80},
                               "chunks": []}))
        w.emit({"type": "turn/ended", "turn": turn, "outcome": "completed"})


def generate(root, workspace, turns=20, tools_per_turn=1, tool_result_bytes=2048, compactions=0, prunes=0,
             forks=0, attempts=0, sessions=1, seed=None, model="fake", text_bytes=200, title=True,
             fork_turns=2, start_ms=None):
    """Write `sessions` root sessions (+ `forks` forks of each) under `root`.
    Returns the list of created session ids (roots first, then forks)."""
    rng = random.Random(seed)
    clock = UlidClock(start_ms, rng)
    root = Path(root)
    root.mkdir(parents=True, exist_ok=True)
    workspace = str(Path(workspace).resolve())
    created, forks_out = [], []
    for s in range(sessions):
        sid = clock.ulid(clock.ms + 1)
        w = Writer(root, sid, clock)
        w.emit({"type": "session/header", "version": 1, "session": sid, "workspace": workspace})
        compact_at = {int((k + 1) * turns / (compactions + 1)) + 1 for k in range(compactions)} if turns > 1 else set()
        visible = []
        _write_turns(w, rng, 1, turns, tools_per_turn, tool_result_bytes, attempts, model, visible, compact_at,
                     [prunes], text_bytes)
        if title:
            w.emit({"type": "session/title", "title": f"synthetic session {s}", "source": "fallback"})
        w.close()
        created.append(sid)
        for f in range(forks):
            # Fork at a turn/ended boundary in the middle of the parent.
            parent_ids = w.ids
            at = parent_ids[max(1, len(parent_ids) * (f + 1) // (forks + 1)) - 1]
            fid = clock.ulid(clock.ms + 1)
            fw = Writer(root, fid, clock)
            fw.emit({"type": "session/header", "version": 1, "session": fid,
                     "parent": {"session": sid, "at": at}, "workspace": workspace})
            _write_turns(fw, rng, turns + 1, fork_turns, tools_per_turn, tool_result_bytes, 0, model, [], set(),
                         [0], text_bytes)
            fw.close()
            forks_out.append(fid)
    return created + forks_out


# -- real corpus sampler ---------------------------------------------------------

def _header(path):
    with open(path, "rb") as f:
        line = f.readline()
    try:
        return json.loads(line)
    except ValueError:
        return {}


def sample_real(dest_root, n=5, source=None, workspace=None, include_ancestors=True, min_bytes=0):
    """COPY the `n` largest sessions from `source` (default ~/.rness/sessions,
    read-only) into `dest_root`, plus their fork ancestors. With `workspace`,
    the copies' header workspace is rewritten so `rness --list` from that
    cwd finds them (only the copy is touched). Returns copied ids, largest first."""
    source = Path(source or Path.home() / ".rness/sessions")
    dest_root = Path(dest_root)
    dest_root.mkdir(parents=True, exist_ok=True)
    sizes = []
    for entry in os.scandir(source):
        log = Path(entry.path) / "session.v1.jsonl"
        try:
            st = log.stat()
        except OSError:
            continue
        if st.st_size >= min_bytes:
            sizes.append((st.st_size, entry.name))
    sizes.sort(reverse=True)
    picked = [sid for _, sid in sizes[:n]]
    todo, done = list(picked), []
    while todo:
        sid = todo.pop(0)
        if sid in done:
            continue
        src = source / sid / "session.v1.jsonl"
        dst_dir = dest_root / sid
        dst_dir.mkdir(exist_ok=True)
        dst = dst_dir / "session.v1.jsonl"
        if workspace:
            with open(src, "rb") as fin, open(dst, "wb") as fout:
                header = json.loads(fin.readline())
                header["workspace"] = str(Path(workspace).resolve())
                fout.write(json.dumps(header, separators=(",", ":")).encode() + b"\n")
                shutil.copyfileobj(fin, fout, 1 << 20)
        else:
            shutil.copyfile(src, dst)
        done.append(sid)
        parent = (_header(src).get("parent") or {}).get("session")
        if include_ancestors and parent and (source / parent / "session.v1.jsonl").is_file():
            todo.append(parent)
    return done


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gen", help="write synthetic sessions")
    g.add_argument("root")
    g.add_argument("--workspace", required=True)
    for name, default in (("turns", 20), ("tools-per-turn", 1), ("tool-result-bytes", 2048), ("compactions", 0),
                          ("prunes", 0), ("forks", 0), ("attempts", 0), ("sessions", 1), ("text-bytes", 200)):
        g.add_argument("--" + name, type=int, default=default)
    g.add_argument("--seed", type=int)
    g.add_argument("--model", default="fake")
    r = sub.add_parser("real", help="copy the N largest real sessions (read-only source)")
    r.add_argument("root")
    r.add_argument("--n", type=int, default=5)
    r.add_argument("--source")
    r.add_argument("--workspace")
    a = ap.parse_args()
    if a.cmd == "gen":
        ids = generate(a.root, a.workspace, a.turns, a.tools_per_turn, a.tool_result_bytes, a.compactions, a.prunes,
                       a.forks, a.attempts, a.sessions, a.seed, a.model, a.text_bytes)
    else:
        ids = sample_real(a.root, a.n, a.source, a.workspace)
    for sid in ids:
        size = (Path(a.root) / sid / "session.v1.jsonl").stat().st_size
        print(f"{sid}\t{size}")


if __name__ == "__main__":
    sys.exit(main())
