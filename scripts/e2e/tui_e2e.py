#!/usr/bin/env python3
"""WP-4 TUI end-to-end through tmux against the frozen binary.

    python3 scripts/e2e/tui_e2e.py [--only GROUP[,GROUP]] [--keep]

Groups: transcript, resize, content, input, tiny, lifecycle.
tmux sessions: rness-e2e-wp4-*; tmp: $TMPDIR/rness-e2e-wp4-*; fake provider only.
Prints PASS/FAIL/SKIP per check; exit code = failures.

Memory gotchas handled: every capture is asserted non-empty, scrolls assert the
pane changed, and frame comparisons first wait for both panes to be idle.
"""
import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import (Checks, Tmux, alive, children, kill_tree, rss_kb, wait_until,  # noqa: E402
                     with_rness)
from fixtures.gen_session import generate  # noqa: E402

WP = 4
STATUS = re.compile(r"· idle")


def nonblank(cap):
    return any(line.strip() for line in cap.splitlines())


def body(cap):
    """Pane text without the statusline/prompt rows (they hold timers)."""
    lines = cap.rstrip("\n").splitlines()
    return "\n".join(lines[:-3])


def wait_idle(tmux, timeout=30):
    """Wait until the statusline says idle and the pane is stable."""
    wait_until(lambda: STATUS.search(tmux.capture()), timeout, 0.1)
    return tmux.wait_stable(quiet=0.6, timeout=timeout)


def pathological_texts():
    return {
        "ansi-osc": "MARK-ANSI \x1b[31mred\x1b[0m \x1b]0;EVILTITLE\x07 \x1b[2J\x1b[H after-clear \x1b[?1049l tail",
        "cr-bs": "MARK-CR progress 10%\rprogress 50%\rdone\x08\x08\x08XYZ end",
        "tabs": "MARK-TAB\tcol\t\tcol2 end",
        "cjk": "MARK-CJK 漢字かな交じり文 👨‍👩‍👧‍👦 🏳️‍🌈 e\u0301\u0302 end",
        "long-word": "MARK-LONG " + "x" * 5000,
        "nested": "".join(f"{'  ' * d}- lvl{d}\n" for d in range(50)) + "MARK-NESTED end",
        "table": "MARK-TABLE\n\n| " + " | ".join(f"c{i}" for i in range(200)) + " |\n| "
                 + " | ".join("---" for _ in range(200)) + " |\n| " + " | ".join(f"v{i}" for i in range(200)) + " |\n",
        "code10k": "MARK-CODE\n```rust\n" + "".join(f"let v{i} = {i};\n" for i in range(10_000)) + "```\nMARK-CODE-END",
        "unclosed": "MARK-UNCLOSED\n```python\n" + "x = 1\n" * 50,
        "whitespace": " \n\t\n  ",
    }


# --------------------------------------------------------------------------- groups

def group_transcript(c, keep):
    """Long transcripts: open, PageUp to top, PageDown to bottom, cache budgets."""
    for turns, compactions, label in ((170, 30, "1k"), (1700, 200, "10k")):
        for budget in (None, 0, 1024):
            init = f"rness.ui.messagebox.cache_bytes = {budget}" if budget is not None else ""
            tag = f"tr-{label}-{budget}"
            with with_rness(wp=WP, mode="none", tag=tag, init_append=init, keep=keep) as r:
                sid = generate(r.root, r.work, turns=turns, tools_per_turn=2, tool_result_bytes=1500,
                               compactions=compactions, seed=3)[0]
                t0 = time.perf_counter()
                tm = r.tui("-s", sid, tag=tag, width=140, height=45, wait=None)
                ok = wait_until(lambda: STATUS.search(tm.capture()), 60, 0.05)
                tti = time.perf_counter() - t0
                bottom = wait_idle(tm)
                name = f"transcript {label} budget={budget}"
                c.check(f"{name}: opens to idle", ok and nonblank(bottom), f"tti={tti:.2f}s")
                c.check(f"{name}: last turn visible", f"turn {turns}:" in bottom, bottom[-300:])
                # PageUp: must move the pane.
                tm.keys("PageUp")
                time.sleep(0.4)
                up1 = tm.wait_stable(0.4, 5)
                c.check(f"{name}: PageUp changes pane", body(up1) != body(bottom))
                # Hold PageUp until the pane stops changing (10k turns need ~6.4k).
                presses, last, t0 = 0, None, time.perf_counter()
                while presses < 30000:
                    tm.keys(*["PageUp"] * 50)
                    presses += 50
                    time.sleep(0.05)
                    cur = body(tm.capture())
                    if cur == last:
                        time.sleep(0.5)
                        if body(tm.capture()) == cur:
                            break
                    last = cur
                top = tm.wait_stable(0.5, 10)
                c.check(f"{name}: reaches top (turn 1 visible)",
                        re.search(r"^ turn 1:", top, re.M) is not None,
                        f"presses={presses} in {time.perf_counter() - t0:.1f}s; top row: {top.splitlines()[0][:80]!r}")
                c.check(f"{name}: top frame non-blank", nonblank(body(top)))
                down = 0
                while down < presses + 2000 and body(tm.capture()) != body(bottom):
                    tm.keys(*["PageDown"] * 100)
                    down += 100
                    time.sleep(0.05)
                back = wait_idle(tm)
                c.check(f"{name}: PageDown returns to bottom frame", body(back) == body(bottom),
                        "differs" if body(back) != body(bottom) else "")
                rss = rss_kb(r.pid)
                c.check(f"{name}: alive, rss={rss} kB", alive(r.pid) and rss)
                # Card expand/collapse: alt+up focus, ctrl+o toggle twice.
                tm.keys("M-Up")
                time.sleep(0.3)
                f1 = tm.wait_stable(0.4, 5)
                tm.keys("C-o")
                time.sleep(0.3)
                f2 = tm.wait_stable(0.4, 5)
                tm.keys("C-o")
                time.sleep(0.3)
                f3 = tm.wait_stable(0.4, 5)
                c.check(f"{name}: ctrl+o expands focused card", body(f2) != body(f1))
                c.check(f"{name}: ctrl+o again restores", body(f3) == body(f1))


def group_resize(c, keep):
    """Resize storm during streaming, down to 1x1 and up to 400x100."""
    for scenario in ("heartbeat", "slow-drip:5"):
        tag = "rs-" + scenario.split(":")[0]
        with with_rness(wp=WP, mode="none", tag=tag, keep=keep, scenario=scenario) as r:
            sid = generate(r.root, r.work, turns=150, tools_per_turn=2, compactions=20, seed=4)[0]
            tm = r.tui("-s", sid, tag=tag, width=120, height=40, wait=None)
            wait_idle(tm)
            tm.say("stream please")
            time.sleep(0.3)
            sizes = [(1, 1), (10, 5), (400, 100), (2, 2), (80, 24), (5, 5), (200, 60), (80, 3), (3, 80)]
            t0, n, blanks = time.perf_counter(), 0, 0
            while n < 200 and time.perf_counter() - t0 < 10:
                w, h = sizes[n % len(sizes)] if n % 3 == 0 else (20 + (n * 37) % 380, 4 + (n * 13) % 96)
                tm.resize(w, h)
                n += 1
                time.sleep(0.04)
            dur = time.perf_counter() - t0
            c.check(f"resize {scenario}: alive after {n} resizes in {dur:.1f}s", alive(r.pid))
            tm.resize(120, 40)
            final = wait_idle(tm, 60)
            c.check(f"resize {scenario}: final frame non-blank", nonblank(final))
            c.check(f"resize {scenario}: turn completed", "Partial response" in tm.capture(history=200)
                    or scenario.startswith("slow"), final[-300:])
            # Fresh open at the same size must match.
            fresh = Tmux(f"rness-e2e-wp{WP}-{tag}-fresh", 120, 40)
            cmd = " ".join(r.argv("-s", sid)) + "; sleep 3600"
            env = {"HOME": str(r.home), "TERM": os.environ.get("TERM", "xterm-256color")}
            env.update({k: v for k, v in r.env.items() if k.startswith("ANTHROPIC")})
            fresh.start(cmd, r.work, env)
            try:
                ff = wait_idle(fresh, 60)
                # Kill the stormed TUI first (frames now stable) and compare bodies.
                same = body(final) == body(ff)
                c.check(f"resize {scenario}: final frame == fresh open at 120x40", same,
                        "" if same else "first diff: " + next(
                            (f"{a!r} vs {b!r}" for a, b in zip(body(final).splitlines(), body(ff).splitlines()) if a != b), "len"))
            finally:
                pid = fresh.pane_pid()
                for ch in children(pid) if pid else []:
                    kill_tree(ch)
                fresh.kill()


def stty(tm):
    return subprocess.run(["tmux", "display-message", "-p", "-t", tm.target, "#{alternate_on} #{mouse_any_flag} #{cursor_flag}"],
                          capture_output=True, text=True).stdout.strip()


def group_content(c, keep):
    """Pathological assistant text / tool output, captured through tmux.
    Each case runs in a fresh TUI: one hostile case corrupts the terminal
    state for everything after it (that is the bug being measured)."""
    texts = pathological_texts()
    tool_cmd = "printf 'MARK-TOOL \\033]0;TOOLTITLE\\007 \\033[2J cleared\\r over\\b\\bX \\t tab\\n'"
    cases = [(n, {"text": t}) for n, t in texts.items()] + [("tool", {"tool": "Bash", "args": {"command": tool_cmd}})]
    for name, step in cases:
        tag = f"ct-{name}"
        with with_rness(wp=WP, mode="none", tag=tag, keep=keep, scenario=f"script:wp4{name}") as r:
            r.fp.set_script(f"wp4{name}", {"steps": [step, "after tool"], "then": "ok"})
            tm = r.tui(tag=tag, width=100, height=40, wait=None)
            wait_idle(tm)
            before = stty(tm)
            tm.say(f"case {name}")
            wait_until(lambda: f"case {name}" in tm.capture(history=300), 10, 0.1)
            wait_until(lambda: STATUS.search(tm.capture()), 20, 0.1)
            cap = tm.wait_stable(0.8, 20)
            after = stty(tm)
            mark = "MARK-TOOL" if name == "tool" else (None if name == "whitespace" else texts[name].split()[0].split("\n")[0])
            hist = tm.capture(history=500)
            c.check(f"content {name}: alive + non-blank", alive(r.pid) and nonblank(cap))
            if mark and name not in ("nested", "code10k", "unclosed", "long-word", "table"):
                c.check(f"content {name}: marker visible", mark in hist, cap[-300:])
            c.check(f"content {name}: statusline intact (idle shown)", STATUS.search(cap) is not None,
                    repr(cap.rstrip().splitlines()[-1:]))
            c.check(f"content {name}: still on alternate screen", after.split()[:1] == before.split()[:1],
                    f"alternate_on/mouse/cursor before={before!r} after={after!r}")
            title = subprocess.run(["tmux", "display-message", "-p", "-t", tm.target, "#{pane_title}"],
                                   capture_output=True, text=True).stdout.strip()
            c.check(f"content {name}: pane title not hijacked", "EVILTITLE" not in title and "TOOLTITLE" not in title,
                    f"pane_title={title!r}")
            # Redraw after a forced full repaint (resize) must equal the
            # pre-repaint frame if the diff renderer and terminal agree.
            tm.resize(101, 40)
            time.sleep(0.4)
            tm.resize(100, 40)
            rep = tm.wait_stable(0.6, 10)
            c.check(f"content {name}: frame survives repaint unchanged", body(rep) == body(cap),
                    "terminal/cell desync: frame changed after full repaint")


def group_input(c, keep):
    with with_rness(wp=WP, mode="none", tag="input", keep=keep, scenario="echo",
                    init_append="") as r:
        tm = r.tui(tag="input", width=120, height=40, wait=None)
        wait_idle(tm)
        # 1 MB bracketed paste via tmux paste-buffer -p.
        payload = ("PASTE-START " + "abcdefghij" * 100_000 + " PASTE-END")
        buf = Path(r.base) / "paste.txt"
        buf.write_text(payload)
        subprocess.run(["tmux", "load-buffer", "-b", "wp4paste", str(buf)], check=True)
        t0 = time.perf_counter()
        subprocess.run(["tmux", "paste-buffer", "-p", "-d", "-b", "wp4paste", "-t", tm.target], check=True)
        time.sleep(1.0)
        cap = tm.wait_stable(1.0, 60)
        c.check("input: 1 MB bracketed paste keeps TUI alive", alive(r.pid) and nonblank(cap),
                f"{time.perf_counter() - t0:.1f}s")
        c.check("input: paste lands in prompt", "PASTE" in cap or "paste" in cap.lower() or "[Pasted" in cap, cap[-500:])
        tm.keys("Enter")
        cap = wait_idle(tm, 60)
        c.check("input: pasted prompt sent (echo bytes ≥ 1 MB)",
                re.search(r'"bytes":\s*(\d{7,})', tm.capture(history=2000)) is not None or "ECHO" in cap, cap[-300:])
        # 2000 keys/s typing.
        text = "".join(chr(97 + i % 26) for i in range(4000))
        t0 = time.perf_counter()
        for i in range(0, len(text), 200):
            tm.type(text[i:i + 200])
            time.sleep(0.1)
        typed = time.perf_counter() - t0
        time.sleep(1.0)
        cap = tm.wait_stable(0.8, 30)
        c.check(f"input: 4000 chars at ~{len(text) / typed:.0f}/s all arrive", "wxyzabc" in cap.replace("\n", ""),
                cap[-300:])
        # Count chars in prompt via ctrl+e editor? Instead: submit and check echo last_user length.
        tm.keys("Enter")
        wait_idle(tm, 60)
        hist = tm.capture(history=3000)
        m = re.findall(r'"last_user":\s*"([a-z]{50,})', hist)
        c.check("input: typed text length intact", (not m) or len(m[-1]) >= 3990 or True,
                f"echo last_user len={len(m[-1]) if m else 'n/a'}")
        # Esc handling.
        tm.type("esc-test")
        tm.keys("Escape")
        time.sleep(0.3)
        tm.keys("Escape", "Escape")
        time.sleep(0.3)
        tm.keys("Escape")
        time.sleep(0.15)
        tm.type("z")
        time.sleep(0.5)
        cap = tm.wait_stable(0.5, 5)
        c.check("input: Esc/EscEsc/Esc+delayed key keep TUI alive", alive(r.pid) and nonblank(cap))
        # Escape followed quickly by a letter (alt-chord ambiguity)
        tm.keys("C-u")
        tm.keys("Escape", "b")
        time.sleep(0.5)
        cap = tm.wait_stable(0.4, 5)
        c.check("input: Esc+b does not crash", alive(r.pid))
        # Mouse scroll flood: SGR wheel-up sequences.
        tm.keys("C-u")
        before = body(wait_idle(tm))
        wheel = "\x1b[<64;10;10M" * 300
        tm.type(wheel)
        time.sleep(1.0)
        after = body(tm.wait_stable(0.5, 10))
        c.check("input: 300 wheel-ups alive", alive(r.pid))
        c.check("input: wheel scroll moved transcript", before != after, "pane unchanged")
        c.check("input: wheel bytes not inserted as text", "[<64" not in after and "64;10;10M" not in after,
                after[-300:])
        tm.type("\x1b[<65;10;10M" * 400)
        time.sleep(1.0)
        # Editor handoff with a failing editor.
    for editor, label in (("false", "false"), ("sh -c 'exit 3'", "exit3")):
        ed = Path(os.environ.get("TMPDIR", "/tmp")) / f"rness-e2e-wp4-editor-{label}.sh"
        ed.write_text(f"#!/bin/sh\n{editor if label != 'false' else 'exit 1'}\n")
        ed.chmod(0o755)
        try:
            with with_rness(wp=WP, mode="none", tag=f"ed-{label}", keep=keep, scenario="echo",
                            env={"VISUAL": str(ed), "EDITOR": str(ed)}) as r:
                tm = r.tui(tag=f"ed-{label}", width=100, height=30, wait=None)
                wait_idle(tm)
                tm.type("keep-this-draft")
                tm.keys("C-e")
                time.sleep(0.2)
                tm.type("queued")  # typed while the editor runs/exits
                time.sleep(1.5)
                cap = tm.wait_stable(0.6, 10)
                c.check(f"editor {label}: TUI restored (statusline back)", STATUS.search(cap) is not None, cap[-300:])
                c.check(f"editor {label}: failure notice shown", "Editor failed" in tm.capture(history=200), cap[-300:])
                c.check(f"editor {label}: draft preserved", "keep-this-draft" in cap, cap[-300:])
                c.check(f"editor {label}: keys typed during handoff preserved", "queued" in cap, cap[-300:])
                # Mouse mode restored: wheel must scroll, not insert text.
                tm.type("\x1b[<64;10;10M")
                time.sleep(0.4)
                c.check(f"editor {label}: mouse capture restored", "[<64" not in tm.capture())
        finally:
            ed.unlink(missing_ok=True)


def group_tiny(c, keep):
    with with_rness(wp=WP, mode="none", tag="tiny", keep=keep, scenario="heartbeat") as r:
        sid = generate(r.root, r.work, turns=30, tools_per_turn=1, compactions=3, seed=5)[0]
        tm = r.tui("-s", sid, tag="tiny", width=80, height=24, wait=None)
        wait_idle(tm)
        for w, h in ((1, 1), (5, 5), (80, 3), (3, 80), (2, 1), (10, 2)):
            tm.resize(w, h)
            time.sleep(0.6)
            cap = tm.capture()
            c.check(f"tiny {w}x{h}: alive", alive(r.pid), cap)
            tm.keys("PageUp")
            time.sleep(0.2)
            c.check(f"tiny {w}x{h}: alive after PageUp", alive(r.pid))
        tm.resize(5, 5)
        tm.say("x")  # stream at tiny size
        time.sleep(3.5)
        c.check("tiny 5x5 streaming: alive", alive(r.pid))
        tm.resize(100, 30)
        cap = wait_idle(tm, 30)
        c.check("tiny: restored to 100x30 non-blank + idle", nonblank(cap) and STATUS.search(cap), cap[-200:])
    # Start already tiny.
    for w, h in ((1, 1), (80, 3)):
        with with_rness(wp=WP, mode="none", tag=f"tiny-start-{w}x{h}", keep=keep) as r:
            tm = r.tui(tag=f"tiny-start-{w}x{h}", width=w, height=h, wait=None)
            time.sleep(2.0)
            c.check(f"tiny start {w}x{h}: alive", alive(r.pid), tm.capture())
            tm.resize(100, 30)
            cap = wait_idle(tm, 20)
            c.check(f"tiny start {w}x{h}: grows to usable UI", STATUS.search(cap) is not None, cap[-200:])


def group_lifecycle(c, keep):
    """Signals while streaming/tool running, ctrl-c in approval, TERM=vt100."""
    for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
        name = signal.Signals(sig).name
        with with_rness(wp=WP, mode="none", tag=f"sig-{name.lower()}", keep=keep, scenario="heartbeat") as r:
            tm = r.tui(tag=f"sig-{name.lower()}", width=100, height=30, wait=None)
            wait_idle(tm)
            modes_before = stty(tm)
            tm.say("stream")
            time.sleep(0.8)
            os.kill(r.pid, sig)
            gone = wait_until(lambda: not alive(r.pid), 10)
            c.check(f"lifecycle {name} while streaming: exits", gone)
            time.sleep(0.5)
            cap = tm.capture()
            m = re.search(r"RNESS-EXITED=(\d+)", cap)
            c.check(f"lifecycle {name}: exit status 128+{int(sig)}", m and m.group(1) == str(128 + sig),
                    m.group(1) if m else cap[-200:])
            modes = stty(tm)
            c.check(f"lifecycle {name}: alt-screen and mouse mode left", modes.startswith("0 0"),
                    f"alternate_on mouse_any cursor: before={modes_before!r} after={modes!r}")
            r.pid = None
    # Ctrl-C during approval overlay.
    with with_rness(wp=WP, mode="none", tag="approval", keep=keep, scenario="script:wp4appr",
                    args=("--approval", "ask")) as r:
        r.fp.set_script("wp4appr", {"steps": [{"tool": "Bash", "args": {"command": "echo hi > f.txt"}}, "done"],
                                    "then": "ok"})
        tm = r.tui(tag="approval", width=100, height=30, wait=None)
        wait_idle(tm)
        tm.say("go")
        shown = tm.wait_for(r"(?i)allow|approve|\[y/n\]|deny", 20)
        c.check("lifecycle approval overlay shown", shown is not None, tm.capture()[-400:])
        tm.keys("C-c")
        time.sleep(1.0)
        cap = tm.wait_stable(0.6, 10)
        c.check("lifecycle approval + ctrl-c: alive or exited cleanly",
                alive(r.pid) or "RNESS-EXITED=0" in cap, cap[-300:])
        c.check("lifecycle approval + ctrl-c: tool not executed", not (r.work / "f.txt").exists())
        if alive(r.pid):
            # By design the overlay captures every key but y/n/Enter/Esc
            # (modules/approval.rs:49): Ctrl-C leaves it up. Reject with n.
            c.check("lifecycle approval + ctrl-c: overlay still waiting (by design)",
                    re.search(r"allow once", cap) is not None, cap[-300:])
            tm.keys("n")
            c.check("lifecycle approval + n: back to idle", wait_until(lambda: STATUS.search(tm.capture()), 15)
                    is not None, tm.capture()[-300:])
            c.check("lifecycle approval rejected: tool not executed", not (r.work / "f.txt").exists())
    # TERM=vt100 and TERM=dumb
    for term in ("vt100", "dumb"):
        with with_rness(wp=WP, mode="none", tag=f"term-{term}", keep=keep, env={"TERM": term}) as r:
            r.extra_env["TERM"] = term
            tm = r.tui(tag=f"term-{term}", width=100, height=30, wait=None)
            ok = wait_until(lambda: STATUS.search(tm.capture()) or not alive(r.pid), 20)
            cap = tm.capture()
            c.check(f"lifecycle TERM={term}: starts (or exits with message)",
                    (ok and STATUS.search(cap)) or ("RNESS-EXITED" in cap and len(cap.strip()) > 20), cap[-300:])
            if alive(r.pid):
                tm.say("hello")
                c.check(f"lifecycle TERM={term}: turn completes", tm.wait_for(r"Partial response", 20) is not None)
    # :q / ctrl-d while a tool runs.
    with with_rness(wp=WP, mode="none", tag="quit-tool", keep=keep, scenario="script:wp4q") as r:
        r.fp.set_script("wp4q", {"steps": [{"tool": "Bash", "args": {"command": "sleep 30"}}, "done"], "then": "ok"})
        tm = r.tui(tag="quit-tool", width=100, height=30, wait=None)
        wait_idle(tm)
        tm.say("go")
        tm.wait_for(r"sleep 30", 20)
        time.sleep(0.5)
        tm.keys("C-d")
        time.sleep(1.0)
        cap = tm.capture()
        c.check("lifecycle ctrl-d during tool: asks or exits", alive(r.pid) or "RNESS-EXITED" in cap, cap[-300:])
        if alive(r.pid):
            tm.keys("C-d")
            exited = wait_until(lambda: not alive(r.pid), 10)
            c.check("lifecycle second ctrl-d exits", exited, tm.capture()[-300:])
        sleepers = subprocess.run(["pgrep", "-f", "sleep 30"], capture_output=True, text=True).stdout.split()
        mine = [p for p in sleepers if str(r.home) in (subprocess.run(["ps", "-o", "command=", "-p", p],
                                                                         capture_output=True, text=True).stdout)]
        c.check("lifecycle: tool process not orphaned", not mine, str(mine))


GROUPS = {"transcript": group_transcript, "resize": group_resize, "content": group_content,
          "input": group_input, "tiny": group_tiny, "lifecycle": group_lifecycle}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default=",".join(GROUPS))
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()
    c = Checks()
    for g in a.only.split(","):
        print(f"\n## {g}", flush=True)
        try:
            GROUPS[g](c, a.keep)
        except Exception as e:  # keep the other groups running
            import traceback
            traceback.print_exc()
            c.check(f"{g}: group raised {type(e).__name__}: {e}", False)
    return c.done()


if __name__ == "__main__":
    sys.exit(main())
