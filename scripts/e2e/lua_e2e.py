#!/usr/bin/env python3
"""WP-3 Lua host e2e against the frozen binary, in a real TUI (tmux).

    python3 scripts/e2e/lua_e2e.py [--only GROUP[,GROUP]] [--keep]

Groups:
  smoke        default flavor boots, statusline renders, a turn completes, clean quit
  hook         §4 #1: runaway `prompt` notification hook mid-session -> what freezes
  statusline   §4 #1: runaway statusline provider at startup / after a reload
  timer        runaway timer (1 s budget) -> statusline stalls about 1 s
  reload       hot reload in the TUI: require cache, broken rewrite, plugin edit applied
tmux: rness-e2e-wp3-*; tmp: $TMPDIR/rness-e2e-wp3-*; fake provider only; ports 8730-8739.
Prints PASS/FAIL/SKIP and JSON probe lines; exit code = failures. These checks
pin the fixed behaviour of plan 04 (Lua execution budgets, B3-1..B3-7).
"""
import argparse
import json
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import Checks, alive, cpu_percent, wait_until, with_rness  # noqa: E402

WP = 3
IDLE = re.compile(r"· idle")
PARTIAL = "Partial response"


def probe(name, **kw):
    print(json.dumps({"probe": name, **kw}), flush=True)


def add_plugin(r, name, source, watch=False):
    """Write ~/.rness/plugins/<name>.lua into the tmp HOME and declare it first.
    A plugin that sets the statusline replaces the flavor's statusline entry
    (one owner; the flavor plugin would otherwise win)."""
    cfg = r.home / ".rness"
    (cfg / "plugins" / f"{name}.lua").write_text(source)
    init = cfg / "init.lua"
    text = init.read_text()
    if "rness.ui.statusline" in source:
        flavor = '  { name = "statusline", file = "plugins/statusline.lua" },\n'
        assert flavor in text
        text = text.replace(flavor, "")
    entry = f'  {{ name = "{name}", file = "plugins/{name}.lua"{", watch = true" if watch else ""} }},\n'
    assert "rness.plugins.setup({\n" in text
    init.write_text(text.replace("rness.plugins.setup({\n", "rness.plugins.setup({\n" + entry, 1))
    return cfg / "plugins" / f"{name}.lua"


def status_row(cap):
    rows = [ln for ln in cap.rstrip("\n").splitlines() if ln.strip()]
    for ln in reversed(rows):
        if "fake" in ln or "idle" in ln or "est tok" in ln:
            return ln.strip()
    return rows[-1].strip() if rows else ""


def quit_tui(tm, r, c, label):
    """Ctrl-D twice; returns seconds to exit or None."""
    t = time.monotonic()
    tm.keys("C-d")
    time.sleep(0.5)
    if alive(r.pid):
        tm.keys("C-d")
    gone = wait_until(lambda: not alive(r.pid), 30)
    took = time.monotonic() - t if gone else None
    if not gone:
        probe(f"{label}_quit_hung", tail=tm.capture().strip()[-200:])
    probe(f"{label}_quit", seconds=None if took is None else round(took, 2))
    return took


def log_tail(r, n=40):
    p = r.home / ".rness" / "log"
    return "\n".join(p.read_text(errors="replace").splitlines()[-n:]) if p.exists() else ""


# ------------------------------------------------------------------ smoke

def group_smoke(c, keep):
    with with_rness(wp=WP, mode="none", tag="smoke", keep=keep) as r:
        t0 = time.monotonic()
        tm = r.tui(tag="smoke", width=120, height=30, wait=None)
        up = tm.wait_for(IDLE, 30)
        probe("smoke_tui_ready", seconds=round(time.monotonic() - t0, 2))
        c.check("smoke: default flavor TUI reaches idle statusline", up is not None, tm.capture()[-500:])
        tm.say("hello")
        done = tm.wait_for(PARTIAL, 20)
        c.check("smoke: turn completes", done is not None, tm.capture()[-500:])
        c.check("smoke: back to idle", wait_until(lambda: IDLE.search(tm.capture()), 15) is not None)
        log = log_tail(r, 200)
        errs = [ln for ln in log.splitlines() if re.search(r"ERROR|lua.*(error|failed)", ln, re.I)]
        c.check("smoke: no Lua errors in ~/.rness/log", not errs, "\n".join(errs[:5]))
        c.check("smoke: quits", quit_tui(tm, r, c, "smoke") is not None)


# ------------------------------------------------------------------ §4 #1 hook

SPIN_PROMPT = """
rness.hook.on("prompt", function(ev)
  if type(ev.text) == "string" and ev.text:find("SPINNOW", 1, true) then
    while true do end
  end
end)
"""


def group_hook(c, keep):
    """Runaway notification hook, triggered by a prompt mid-session."""
    with with_rness(wp=WP, mode="none", tag="hook", keep=keep, scenario="script:wp3hook") as r:
        # turn 1 fast; later turns drip slowly so the statusline has a busy window
        r.fp.set_script("wp3hook", {"steps": [PARTIAL + " one", {"text": "second-answer", "delay_ms": 5000}]})
        add_plugin(r, "wp3spin", SPIN_PROMPT)
        tm = r.tui(tag="hook", width=120, height=30, wait=None)
        c.check("hook: TUI idle before trigger", tm.wait_for(IDLE, 30) is not None, tm.capture()[-400:])
        tm.say("first SPINNOW")
        # Does the turn itself still run? (prompt hook is fire-and-forget)
        turn = tm.wait_for(PARTIAL, 20)
        c.check("hook: turn after runaway prompt hook still streams (provider path has no Lua)",
                turn is not None, tm.capture()[-400:])
        time.sleep(2)
        idle = IDLE.search(tm.capture())
        c.check("hook: statusline returns to idle after the turn",
                idle is not None, status_row(tm.capture()))
        rows = []
        for _ in range(4):
            rows.append(status_row(tm.capture()))
            time.sleep(1)
        probe("hook_statusline_rows", rows=rows)
        cpu = cpu_percent(r.pid)
        probe("hook_cpu_percent", value=cpu)
        c.check("hook: rness not pinned at ~100% CPU by the runaway handler",
                cpu is not None and cpu < 80, f"cpu={cpu}")
        # Input still echoes? (TUI thread independent of the VM)
        tm.type("typed-while-wedged")
        echoed = tm.wait_for("typed-while-wedged", 5)
        c.check("hook: keyboard input still echoes (TUI loop alive)", echoed is not None)
        tm.keys("C-u")
        tm.keys("C-a", "C-k")
        time.sleep(0.3)
        # A second, slow turn: does the statusline ever show it as busy?
        tm.say("second turn")
        t = time.monotonic()
        busy_seen = False
        while time.monotonic() - t < 25:
            cap = tm.capture(history=200)
            if re.search(r"working|writing|thinking", status_row(tm.capture())):
                busy_seen = True
            if "second-answer" in cap:
                break
            time.sleep(0.2)
        second = "second-answer" in tm.capture(history=200)
        probe("hook_second_turn", completed=second, statusline_busy_seen=busy_seen,
              seconds=round(time.monotonic() - t, 2))
        c.check("hook: a later turn completes while the VM is wedged (no Lua hooks on the turn path)", second,
                tm.capture()[-500:])
        c.check("hook: statusline shows the later turn as busy", busy_seen,
                status_row(tm.capture()))
        # A Lua slash command needs the VM. "Command running" is only the
        # admission ack; /project then prints the work dir.
        tm.say("/project")
        time.sleep(5)
        cap = tm.capture(history=60)
        probe("hook_lua_command_tail", tail=cap.strip()[-200:])
        stuck = "Command running" in cap and str(r.base.name) not in cap.rsplit("Command running", 1)[-1]
        c.check("hook: Lua slash command (/project) completes after a runaway hook", not stuck, cap[-300:])
        if stuck:
            tm.keys("C-c")
            time.sleep(1)
            cap = tm.capture(history=60)
            probe("hook_lua_command_after_ctrl_c", still_running="Command running" in cap.strip()[-200:],
                  tail=cap.strip()[-160:])
        took = quit_tui(tm, r, c, "hook")
        c.check("hook: Ctrl-D quits a wedged-VM rness", took is not None, tm.capture()[-300:])


# ------------------------------------------------------------------ §4 #1 statusline

SPIN_STATUS_STARTUP = """
rness.ui.statusline(function() while true do end end)
"""

SPIN_STATUS_LATER = """
rness.ui.statusline(function(ctx)
  local f = io.open(%r, "r")
  if f then f:close(); while true do end end
  return "WP3-STATUS-OK"
end)
"""


def group_statusline(c, keep):
    # (1) runaway at startup: status_text.prime() runs before the first frame.
    with with_rness(wp=WP, mode="none", tag="status-start", keep=keep) as r:
        add_plugin(r, "wp3status", SPIN_STATUS_STARTUP)
        tm = r.tui(tag="status-start", width=120, height=30, wait=None)
        time.sleep(12)
        cap = tm.capture()
        drawn = bool(cap.strip()) and ("fake" in cap or "idle" in cap or "›" in cap or ">" in cap)
        probe("statusline_startup_drawn_after_12s", drawn=drawn, cpu=cpu_percent(r.pid),
              tail=cap.strip()[-120:])
        c.check("statusline: runaway statusline at startup still lets the TUI draw", drawn, cap[-300:])
        if alive(r.pid):
            t = time.monotonic()
            tm.keys("C-c")
            tm.keys("C-d")
            gone = wait_until(lambda: not alive(r.pid), 5)
            probe("statusline_startup_ctrl_c_exit", exited=bool(gone),
                  seconds=round(time.monotonic() - t, 2) if gone else None)
            c.check("statusline: Ctrl-C/Ctrl-D exits a startup-wedged rness", gone)
    # (2) runaway begins mid-session (flag file).
    with with_rness(wp=WP, mode="none", tag="status-later", keep=keep) as r:
        flag = r.base / "SPIN"
        add_plugin(r, "wp3status", SPIN_STATUS_LATER % str(flag))
        tm = r.tui(tag="status-later", width=120, height=30, wait=None)
        c.check("statusline: custom provider renders", tm.wait_for("WP3-STATUS-OK", 30) is not None,
                tm.capture()[-300:])
        flag.write_text("1")
        time.sleep(2)
        tm.say("hello after statusline wedge")
        turn = tm.wait_for(PARTIAL, 25)
        c.check("statusline: turn completes after statusline provider wedged the VM",
                turn is not None, tm.capture()[-400:])
        tm.type("still-typing")
        c.check("statusline: input still echoes", tm.wait_for("still-typing", 5) is not None)
        probe("statusline_later_cpu", value=cpu_percent(r.pid))
        c.check("statusline: Ctrl-D quits", quit_tui(tm, r, c, "statusline_later") is not None)


# ------------------------------------------------------------------ timer

SPIN_TIMER = """
local ticks = 0
rness.timer.every(1, function() ticks = ticks + 1 end)
rness.ui.statusline(function() return "WP3-TICK-" .. ticks end)
rness.hook.on("prompt", function(ev)
  if ev.text:find("TIMERSPIN", 1, true) then
    rness.timer.after(0.1, function() while true do end end)
  end
end)
"""


def group_timer(c, keep):
    with with_rness(wp=WP, mode="none", tag="timer", keep=keep) as r:
        add_plugin(r, "wp3timer", SPIN_TIMER)
        tm = r.tui(tag="timer", width=120, height=30, wait=None)
        c.check("timer: tick statusline renders", tm.wait_for(r"WP3-TICK-[1-9]", 30) is not None,
                tm.capture()[-300:])
        tm.say("go TIMERSPIN")
        tm.wait_for(PARTIAL, 20)
        # Watch the tick counter: it freezes while the runaway timer holds the VM.
        seen, t0 = [], time.monotonic()
        last, frozen_since, longest = None, None, 0.0
        while time.monotonic() - t0 < 45:
            m = re.search(r"WP3-TICK-(\d+)", tm.capture())
            v = int(m.group(1)) if m else None
            now = time.monotonic()
            if v != last:
                if frozen_since is not None:
                    longest = max(longest, now - frozen_since)
                frozen_since, last = now, v
                seen.append((round(now - t0, 1), v))
            time.sleep(0.25)
        if frozen_since is not None:
            longest = max(longest, time.monotonic() - frozen_since)
        probe("timer_statusline_longest_freeze_s", value=round(longest, 1), samples=seen[-6:])
        c.check("timer: statusline resumes after the runaway timer", longest < 40, f"longest={longest}")
        c.check("timer: statusline freeze under 5 s (B3-7)", longest < 5, f"froze {longest:.1f}s")
        quit_tui(tm, r, c, "timer")


# ------------------------------------------------------------------ reload

RELOAD_PLUGIN = """
local h = require("wp3helper")
rness.ui.statusline(function() return "WP3-HV-" .. h.v .. "-%s" end)
"""


def group_reload(c, keep):
    with with_rness(wp=WP, mode="none", tag="reload", keep=keep) as r:
        cfg = r.home / ".rness"
        (cfg / "lua" / "wp3helper.lua").write_text("return { v = 'one' }")
        plugin = add_plugin(r, "wp3reload", RELOAD_PLUGIN % "a", watch=True)
        tm = r.tui(tag="reload", width=140, height=30, wait=None)
        c.check("reload: initial view", tm.wait_for("WP3-HV-one-a", 30) is not None, tm.capture()[-300:])
        time.sleep(1.5)  # watcher init
        # plugin edit applied live
        t = time.monotonic()
        plugin.write_text(RELOAD_PLUGIN % "b")
        ok = tm.wait_for("WP3-HV-one-b", 10)
        probe("reload_plugin_edit_visible_s", value=round(time.monotonic() - t, 2) if ok else None)
        c.check("reload: plugin edit applied live", ok is not None, tm.capture()[-300:])
        # helper (lua/) edit: needs restart; plugin keeps the cached module
        (cfg / "lua" / "wp3helper.lua").write_text("return { v = 'two' }")
        time.sleep(1.5)
        cap = tm.capture(history=100)
        probe("reload_helper_edit_notice", restart_notice=bool(re.search(r"restart", cap, re.I)))
        c.check("reload: lua/ helper edit is not applied live", "WP3-HV-one-b" in cap, status_row(cap))
        plugin.write_text(RELOAD_PLUGIN % "c")
        ok = tm.wait_for(r"WP3-HV-\w+-c", 10)
        shown = re.search(r"WP3-HV-(\w+)-c", tm.capture())
        probe("reload_require_cache", helper=shown.group(1) if shown else None)
        c.check("reload: require cache persists across plugin reload (documented)",
                shown and shown.group(1) == "one", tm.capture()[-200:])
        # broken rewrite: previous statusline kept, error surfaced
        plugin.write_text("this is not lua")
        time.sleep(1.5)
        cap = tm.capture(history=100)
        c.check("reload: broken rewrite keeps previous statusline", "WP3-HV-one-c" in cap, status_row(cap))
        log = log_tail(r, 30)
        probe("reload_broken_notice", in_pane=bool(re.search(r"(?i)reload|error|syntax|wp3reload", cap)),
              in_log=bool(re.search(r"(?i)wp3reload|reload", log)))
        c.check("reload: broken rewrite is reported to the user (pane or log)",
                re.search(r"(?i)reload|error|syntax|wp3reload", cap + log), log[-300:])
        # runaway rewrite: hot reload of `while true do end` (B3-3). The 5 s load
        # budget stops it; turns submitted meanwhile are rejected as busy.
        plugin.write_text("while true do end")
        time.sleep(8)
        tm.say("turn after runaway reload")
        turn = tm.wait_for(PARTIAL, 20)
        cpu = cpu_percent(r.pid)
        probe("reload_runaway", turn_completed=turn is not None, cpu=cpu, status=status_row(tm.capture()))
        c.check("reload: turn still completes after a runaway hot reload", turn is not None, tm.capture()[-300:])
        c.check("reload: a runaway plugin chunk on hot reload does not pin a core forever",
                cpu is not None and cpu < 80, f"cpu={cpu}")
        plugin.write_text(RELOAD_PLUGIN % "d")
        time.sleep(3)
        c.check("reload: fixing the runaway plugin file recovers the session",
                "WP3-HV-one-d" in tm.capture(), status_row(tm.capture()))
        quit_tui(tm, r, c, "reload")


GROUPS = {"smoke": group_smoke, "hook": group_hook, "statusline": group_statusline,
          "timer": group_timer, "reload": group_reload}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only")
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()
    c = Checks()
    for name in (a.only.split(",") if a.only else GROUPS):
        print(f"\n== {name}", flush=True)
        try:
            GROUPS[name](c, a.keep)
        except Exception as e:  # keep going; report as failure
            c.check(f"{name}: group raised", False, repr(e))
    sys.exit(c.done())


if __name__ == "__main__":
    main()
