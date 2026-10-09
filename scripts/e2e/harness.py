#!/usr/bin/env python3
"""Process harness for rness E2E/perf scripts. Standard library only.

    import sys; sys.path.insert(0, "scripts/e2e")
    from harness import *
    with with_rness(wp=1, scenario="echo") as r:          # headless (default)
        res = r.headless("hello")                          # -> Result
        assert res.rc == 0 and "ECHO" in res.stdout
    with with_rness(wp=1, mode="tui", scenario="ok") as r: # TUI in tmux
        r.tmux.say("hi"); r.tmux.wait_for("Partial response")
    with with_rness(wp=1, mode="serve") as r:              # --serve
        sid = r.api("POST", "/api/sessions", {})["session"]

See scripts/e2e/README.md for the full API.
"""
import contextlib
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(REPO / "scripts"))
from fake_provider import FakeProvider  # noqa: E402

BIN_DIR = Path.home() / ".cache/rness-e2e/bin"
BIN = Path(os.environ.get("RNESS_E2E_BIN", BIN_DIR / "rness"))
BIN_DEBUG = Path(os.environ.get("RNESS_E2E_BIN_DEBUG", BIN_DIR / "rness-debug"))
TMPDIR = Path(os.environ.get("TMPDIR", "/tmp"))
SHAPES = ("anthropic", "openai", "responses")


# -- naming conventions ----------------------------------------------------

def wp_port(wp, slot=0):
    """Fixed port for WP `wp`, slot 0-9 (WP-0: 8700-8709)."""
    assert 0 <= slot <= 9
    return 8700 + 10 * wp + slot


def free_wp_port(wp):
    """First bindable port in the WP's range."""
    for slot in range(10):
        port = wp_port(wp, slot)
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", port))
                return port
            except OSError:
                continue
    raise RuntimeError(f"no free port in 87{wp}0-87{wp}9")


def tmp_dir(wp, tag="run"):
    """New private dir $TMPDIR/rness-e2e-wpN-<tag>-XXXX (caller removes it)."""
    return Path(tempfile.mkdtemp(prefix=f"rness-e2e-wp{wp}-{tag}-", dir=TMPDIR))


def start_provider(wp=0, port=None, **kw):
    """Start a FakeProvider on the WP's first free port (or `port`, 0 = any)."""
    if port is None:
        last = None
        for slot in range(10):
            try:
                return FakeProvider(port=wp_port(wp, slot), **kw).start()
            except OSError as e:
                last = e
        raise last
    return FakeProvider(port=port, **kw).start()


# -- isolated HOME -----------------------------------------------------------

def make_home(base, flavor=True, init_append="", credentials=None, allow_generic_agents=False):
    """Create `base`/home with the default flavor installed (as install.sh
    does: cp -R flavors/default/. ~/.rness) and a `work` cwd. Returns
    (home, work). `init_append` is Lua appended to ~/.rness/init.lua."""
    home = Path(base) / "home"
    work = Path(base) / "work"
    work.mkdir(parents=True, exist_ok=True)
    cfg = home / ".rness"
    if flavor:
        shutil.copytree(REPO / "flavors/default", cfg)
        (cfg / ".DS_Store").unlink(missing_ok=True)
    else:
        cfg.mkdir(parents=True)
    os.chmod(cfg, 0o700)
    if init_append:
        with open(cfg / "init.lua", "a") as f:
            f.write("\n" + init_append + "\n")
    if allow_generic_agents:
        agents = cfg / "lua/agents.lua"
        agents.write_text(agents.read_text().replace("rness.agents.allow_generic = false",
                                                     "rness.agents.allow_generic = true"))
    if credentials:
        p = cfg / "credentials.json"
        p.write_text(json.dumps(credentials))
        os.chmod(p, 0o600)
    return home, work


# Fake ChatGPT OAuth tokens: the Responses route only works with OAuth.
RESPONSES_CREDENTIALS = {"providers": {"openai-chatgpt": {"oauthTokens": {
    "accessToken": "fake-local-only", "refreshToken": "", "accountId": "acct-fake"}}}}


def provider_flags(fp, shape="anthropic", scenario="ok", model="fake"):
    """(argv, env) that point rness at the fake provider for `shape`.

    anthropic: -m anthropic/<model> --base-url http://H:P/<scenario>  + ANTHROPIC_API_KEY
    openai:    --route fake=http://H:P/<scenario>/v1,none -m fake/<model>
    responses: -m chatgpt/<model> --base-url http://H:P/<scenario>   + fake OAuth file (make_home)
    """
    if shape == "anthropic":
        return ["-m", f"anthropic/{model}", "--base-url", fp.anthropic_url(scenario)], {"ANTHROPIC_API_KEY": "fake-local-only"}
    if shape == "openai":
        return ["--route", f"fake={fp.openai_url(scenario)},none", "-m", f"fake/{model}"], {}
    if shape == "responses":
        return ["-m", f"chatgpt/{model}", "--base-url", fp.responses_url(scenario)], {}
    raise ValueError(shape)


# -- results / logs ------------------------------------------------------------

class Result:
    def __init__(self, rc, stdout, stderr, elapsed):
        self.rc, self.stdout, self.stderr, self.elapsed = rc, stdout, stderr, elapsed
        m = re.search(r"^session: (\S+)\s*$", stderr, re.M)
        self.session = m.group(1) if m else None

    @property
    def attempts(self):
        """Parsed `[attempt: ...]` stderr lines (Debug-formatted outcomes)."""
        return re.findall(r"^\[attempt: (.*)\]$", self.stderr, re.M)

    def __repr__(self):
        return f"Result(rc={self.rc}, {self.elapsed:.2f}s, session={self.session}, stdout={self.stdout[-300:]!r}, stderr={self.stderr[-600:]!r})"


def read_log(root, session):
    """All envelopes of <root>/<session>/session.v1.jsonl as dicts."""
    path = Path(root) / session / "session.v1.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def event_types(root, session):
    return [e["type"] for e in read_log(root, session)]


def list_session_dirs(root):
    root = Path(root)
    return sorted(p.name for p in root.iterdir() if (p / "session.v1.jsonl").is_file()) if root.is_dir() else []


# -- process inspection ------------------------------------------------------

def alive(pid):
    if not pid:
        return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    # Zombies count as dead.
    out = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return bool(out) and not out.startswith("Z")


def rss_kb(pid):
    """Resident set size in KiB (ps), None if gone."""
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out) if out.isdigit() else None


def footprint_kb(pid):
    """macOS phys_footprint in KiB (what Activity Monitor shows); falls back to RSS."""
    if sys.platform != "darwin" or not shutil.which("footprint"):
        return rss_kb(pid)
    out = subprocess.run(["footprint", "-p", str(pid)], capture_output=True, text=True).stdout
    m = re.search(r"Footprint:\s*([\d.]+)\s*(KB|MB|GB|B)", out)
    if not m:
        return rss_kb(pid)
    mult = {"B": 1 / 1024, "KB": 1, "MB": 1024, "GB": 1024 * 1024}[m.group(2)]
    return int(float(m.group(1)) * mult)


def cpu_percent(pid):
    out = subprocess.run(["ps", "-o", "%cpu=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    try:
        return float(out)
    except ValueError:
        return None


def fd_count(pid):
    """Open file descriptors (lsof -p, minus the header); None if gone."""
    if sys.platform.startswith("linux"):
        try:
            return len(os.listdir(f"/proc/{pid}/fd"))
        except OSError:
            return None
    out = subprocess.run(["lsof", "-n", "-P", "-p", str(pid)], capture_output=True, text=True).stdout
    lines = [l for l in out.splitlines()[1:] if l.split() and l.split()[3][:1].isdigit()]
    return len(lines) if out else None


def _ps_table():
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,command="], capture_output=True, text=True).stdout
    table = {}
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) >= 2 and parts[0].isdigit():
            table[int(parts[0])] = (int(parts[1]), parts[2] if len(parts) > 2 else "")
    return table


def children(pid):
    """Direct child pids (one `ps` snapshot; same answer as pgrep -P)."""
    return sorted(p for p, (pp, _) in _ps_table().items() if pp == pid)


def process_tree(pid):
    """[(pid, depth, command)] for pid and all descendants, pre-order, from one ps snapshot."""
    table = _ps_table()
    kids = {}
    for p, (pp, _) in table.items():
        kids.setdefault(pp, []).append(p)
    out, seen = [], set()

    def walk(p, depth):
        if p in seen:
            return
        seen.add(p)
        out.append((p, depth, table.get(p, (0, ""))[1]))
        for c in sorted(kids.get(p, [])):
            walk(c, depth + 1)
    walk(pid, 0)
    return out


def descendants(pid):
    return [p for p, d, _ in process_tree(pid) if d > 0]


def kill9(pid):
    with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGKILL)


def kill_tree(pid, sig=signal.SIGKILL):
    for p in reversed([p for p, _, _ in process_tree(pid)]):
        with contextlib.suppress(ProcessLookupError):
            os.kill(p, sig)


def wait_until(pred, timeout=15.0, interval=0.05):
    """Poll `pred()` until truthy; returns its value or None on timeout."""
    end = time.monotonic() + timeout
    while True:
        v = pred()
        if v:
            return v
        if time.monotonic() >= end:
            return None
        time.sleep(interval)


def kill9_when(pid, pred, timeout=30.0, interval=0.01):
    """kill -9 `pid` the moment `pred()` becomes true. Returns True if fired."""
    if wait_until(pred, timeout, interval):
        kill9(pid)
        return True
    return False


def log_grew_to(path, pattern):
    """Predicate factory: file at `path` contains regex `pattern`."""
    rx = re.compile(pattern)
    return lambda: Path(path).exists() and bool(rx.search(Path(path).read_text(errors="replace")))


class Sampler:
    """Background RSS/fd/cpu sampler: `with Sampler(pid, every=1.0) as s: ...; s.samples`."""

    def __init__(self, pid, every=1.0, fds=True, footprint=False):
        import threading
        self.pid, self.every, self.fds, self.footprint = pid, every, fds, footprint
        self.samples = []
        self._stop = threading.Event()
        self._t = threading.Thread(target=self._run, daemon=True)

    def _run(self):
        t0 = time.monotonic()
        while not self._stop.is_set() and alive(self.pid):
            s = {"t": round(time.monotonic() - t0, 3), "rss_kb": rss_kb(self.pid), "cpu": cpu_percent(self.pid)}
            if self.fds:
                s["fds"] = fd_count(self.pid)
            if self.footprint:
                s["footprint_kb"] = footprint_kb(self.pid)
            self.samples.append(s)
            self._stop.wait(self.every)

    def __enter__(self):
        self._t.start()
        return self

    def __exit__(self, *exc):
        self._stop.set()
        self._t.join(timeout=10)

    def peak(self, key="rss_kb"):
        vals = [s[key] for s in self.samples if s.get(key) is not None]
        return max(vals) if vals else None


class Timer:
    """`with Timer() as t: ...; t.ms`"""

    def __enter__(self):
        self.t0 = time.perf_counter()
        return self

    def __exit__(self, *exc):
        self.ms = (time.perf_counter() - self.t0) * 1000

    @property
    def elapsed_ms(self):
        return (time.perf_counter() - self.t0) * 1000


# -- tmux ------------------------------------------------------------------------

def _tmux(*args, check=False):
    return subprocess.run(["tmux", *args], capture_output=True, text=True, check=check)


class Tmux:
    """A detached tmux session named rness-e2e-wpN-<tag>. Only ever touches its own session."""

    def __init__(self, name, width=160, height=50):
        assert name.startswith("rness-e2e-wp"), "tmux sessions must be named rness-e2e-wpN-*"
        self.name, self.width, self.height = name, width, height

    def start(self, command, cwd, env=None):
        _tmux("kill-session", "-t", "=" + self.name)
        prefix = " ".join(f"{k}={_shq(v)}" for k, v in (env or {}).items())
        cmd = f"{'env ' + prefix + ' ' if prefix else ''}{command}"
        _tmux("new-session", "-d", "-s", self.name, "-x", str(self.width), "-y", str(self.height),
              "-c", str(cwd), cmd, check=True)
        _tmux("set-option", "-t", self.name, "remain-on-exit", "on")
        return self

    @property
    def target(self):
        return "=" + self.name + ":"

    def capture(self, history=0, escapes=False):
        args = ["capture-pane", "-p", "-t", self.target]
        if history:
            args += ["-S", f"-{history}"]
        if escapes:
            args.append("-e")
        return _tmux(*args).stdout

    def wait_for(self, pattern, timeout=15.0, history=0):
        """Wait until regex `pattern` is visible; returns the match or None."""
        rx = re.compile(pattern)
        return wait_until(lambda: rx.search(self.capture(history)), timeout, 0.2)

    def wait_stable(self, quiet=0.5, timeout=10.0):
        """Wait until the pane stops changing for `quiet` seconds."""
        last, since = None, time.monotonic()
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            cur = self.capture()
            if cur != last:
                last, since = cur, time.monotonic()
            elif time.monotonic() - since >= quiet:
                return cur
            time.sleep(0.1)
        return last

    def keys(self, *keys):
        """tmux send-keys (key names: Enter, Escape, C-c, Up, ...)."""
        _tmux("send-keys", "-t", self.target, *keys)

    def type(self, text):
        _tmux("send-keys", "-t", self.target, "-l", text)

    def say(self, text, settle=0.3):
        """Type literal text then press Enter."""
        self.type(text)
        time.sleep(settle)
        self.keys("Enter")

    def resize(self, width, height):
        _tmux("resize-window", "-t", self.target, "-x", str(width), "-y", str(height))

    def pane_pid(self):
        out = _tmux("display-message", "-p", "-t", self.target, "#{pane_pid}").stdout.strip()
        return int(out) if out.isdigit() else None

    def pane_dead(self):
        return _tmux("display-message", "-p", "-t", self.target, "#{pane_dead}").stdout.strip() == "1"

    def exists(self):
        return _tmux("has-session", "-t", "=" + self.name).returncode == 0

    def kill(self):
        _tmux("kill-session", "-t", "=" + self.name)


def _shq(s):
    s = str(s)
    return s if re.fullmatch(r"[\w./:=,@%+-]+", s) else "'" + s.replace("'", "'\\''") + "'"


# -- the rness wrapper -------------------------------------------------------------

class Rness:
    """Everything needed to run rness against a fake provider in a tmp HOME."""

    def __init__(self, base, home, work, fp, shape, scenario, binary, extra_args, env, wp, model):
        self.base, self.home, self.work, self.fp = Path(base), Path(home), Path(work), fp
        self.shape, self.scenario, self.binary, self.wp, self.model = shape, scenario, Path(binary), wp, model
        self.extra_args = list(extra_args)
        self.root = self.home / ".rness/sessions"  # default session root
        self.extra_env = dict(env or {})
        self.procs = []
        self.tmux = None
        self.pid = None  # rness pid for tui/serve modes
        self.serve_addr = None
        self.control_socket = None

    @property
    def env(self):
        _, penv = provider_flags(self.fp, self.shape, self.scenario, self.model)
        env = {k: v for k, v in os.environ.items() if not k.startswith(("ANTHROPIC_", "OPENAI_", "OPENROUTER_", "RNESS_"))}
        env.update(HOME=str(self.home), TERM=os.environ.get("TERM", "xterm-256color"), NO_COLOR="1")
        env.update(penv)
        env.update(self.extra_env)
        return env

    def argv(self, *args, scenario=None, provider=True):
        """Full argv: binary + provider flags + --instructions none + extra + args."""
        pflags = provider_flags(self.fp, self.shape, scenario or self.scenario, self.model)[0] if provider and self.fp else []
        return [str(self.binary), *pflags, "--instructions", "none", *self.extra_args, *args]

    def run(self, *args, timeout=60, input=None, provider=True, scenario=None, cwd=None, env=None):
        """Run rness to completion with arbitrary args. Returns Result."""
        t0 = time.perf_counter()
        e = self.env
        e.update(env or {})
        p = subprocess.run(self.argv(*args, scenario=scenario, provider=provider), cwd=cwd or self.work,
                           env=e, capture_output=True, text=True, timeout=timeout, input=input)
        return Result(p.returncode, p.stdout, p.stderr, time.perf_counter() - t0)

    def headless(self, prompt, *args, session=None, timeout=60, scenario=None):
        """`rness -p prompt [-s session]` -> Result (rc, stdout, stderr, session, elapsed, attempts)."""
        extra = ["-s", session] if session else []
        return self.run("-p", prompt, *extra, *args, timeout=timeout, scenario=scenario)

    def spawn(self, *args, scenario=None, provider=True, stdin=subprocess.DEVNULL):
        """Popen rness (stdout/stderr to files under base/). Returns the Popen; tracked for cleanup."""
        n = len(self.procs)
        out = open(self.base / f"proc{n}.out", "w")
        err = open(self.base / f"proc{n}.err", "w")
        p = subprocess.Popen(self.argv(*args, scenario=scenario, provider=provider), cwd=self.work, env=self.env,
                             stdin=stdin, stdout=out, stderr=err, start_new_session=True)
        p.out_path, p.err_path = self.base / f"proc{n}.out", self.base / f"proc{n}.err"
        self.procs.append(p)
        return p

    def spawn_headless(self, prompt, *args, scenario=None):
        return self.spawn("-p", prompt, *args, scenario=scenario)

    def tui(self, *args, tag="tui", width=160, height=50, wait=r"fake|Model|>", timeout=60):
        """Start the TUI in tmux session rness-e2e-wpN-<tag>; sets self.tmux / self.pid."""
        self.tmux = Tmux(f"rness-e2e-wp{self.wp}-{tag}", width, height)
        cmd = " ".join(_shq(a) for a in self.argv(*args)) + "; echo RNESS-EXITED=$?; sleep 3600"
        env = {k: v for k, v in self.env.items() if k in ("HOME", "TERM", "NO_COLOR") or k not in os.environ
               or os.environ[k] != v}
        env.pop("NO_COLOR", None)
        self.tmux.start(cmd, self.work, env)
        shell = self.tmux.pane_pid()
        self.pid = wait_until(lambda: next(iter(children(shell)), None), 10)
        if wait:
            self.tmux.wait_for(wait, timeout)
        return self.tmux

    def serve(self, *args, port=None, timeout=30):
        """Start `--serve 127.0.0.1:<port>` (port from the WP range). Returns the Popen."""
        port = port or free_wp_port(self.wp)
        self.serve_addr = f"127.0.0.1:{port}"
        p = self.spawn("--serve", self.serve_addr, *args)
        self.pid = p.pid
        ok = wait_until(lambda: "rness serving on" in Path(p.err_path).read_text() or p.poll() is not None, timeout)
        if not ok or p.poll() is not None:
            raise RuntimeError(f"rness --serve failed: {Path(p.err_path).read_text()[-2000:]}")
        return p

    def api(self, method, path, body=None, timeout=30, raw=False):
        """JSON request against the --serve instance. Returns parsed JSON (or (status, text) if raw)."""
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(f"http://{self.serve_addr}{path}", data=data, method=method,
                                     headers={"Content-Type": "application/json"} if data else {})
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                text = resp.read().decode()
                status = resp.status
        except urllib.error.HTTPError as e:
            text, status = e.read().decode(), e.code
            if not raw:
                raise RuntimeError(f"{method} {path} -> {status}: {text}")
        if raw:
            return status, text
        return json.loads(text) if text.strip() else None

    def sse(self, path="/api/events", timeout=30):
        """Iterator of (event, data) from an SSE endpoint of the --serve instance."""
        resp = urllib.request.urlopen(f"http://{self.serve_addr}{path}", timeout=timeout)
        event, data = None, []
        for line in resp:
            line = line.decode().rstrip("\n").rstrip("\r")
            if not line:
                if data:
                    yield event, "\n".join(data)
                event, data = None, []
            elif line.startswith("event:"):
                event = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].lstrip())

    def sessions(self):
        return list_session_dirs(self.root)

    def log(self, session):
        return read_log(self.root, session)

    def latest_session(self):
        s = self.sessions()
        return s[-1] if s else None

    def cleanup(self):
        if self.tmux:
            if self.pid:
                kill_tree(self.pid)
            self.tmux.kill()
        for p in self.procs:
            if p.poll() is None:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(p.pid, signal.SIGKILL)
                with contextlib.suppress(Exception):
                    p.wait(5)


@contextlib.contextmanager
def with_rness(wp=0, mode="headless", scenario="ok", shape="anthropic", binary=None, debug=False,
               provider=None, args=(), env=None, init_append="", flavor=True, keep=False, tag=None,
               model="fake", allow_generic_agents=False, record=False, tui_wait=r"fake", **provider_kw):
    """Tmp HOME (default flavor) + fake provider + rness in `mode`:
       headless -> yields Rness; call r.headless(prompt) / r.run(...) / r.spawn_headless(...)
       tui      -> TUI started in tmux `rness-e2e-wp<wp>-<tag or 'tui'>`; r.tmux, r.pid
       serve    -> `--serve` on a WP port; r.api(...), r.sse(...), r.pid
       none     -> nothing started (use r.run / r.spawn yourself)
    Pass `provider=` to share one FakeProvider across runs (it is not stopped then).
    Everything (processes, tmux session, tmp dir unless keep=True) is cleaned up on exit."""
    base = tmp_dir(wp, tag or mode)
    own_fp = provider is None
    fp = provider or start_provider(wp, record=str(base / "requests") if record else None, **provider_kw)
    creds = RESPONSES_CREDENTIALS if shape == "responses" else None
    home, work = make_home(base, flavor=flavor, init_append=init_append, credentials=creds,
                           allow_generic_agents=allow_generic_agents)
    r = Rness(base, home, work, fp, shape, scenario, binary or (BIN_DEBUG if debug else BIN), args, env, wp, model)
    try:
        if mode == "tui":
            r.tui(tag=tag or "tui", wait=tui_wait)
        elif mode == "serve":
            r.serve()
        yield r
    finally:
        r.cleanup()
        if own_fp:
            fp.stop()
        if not keep:
            shutil.rmtree(base, ignore_errors=True)


# -- check reporting (PASS/FAIL per check; exit code = failures) -----------------

class Checks:
    def __init__(self):
        self.failures = 0
        self.results = []

    def check(self, name, ok, detail=""):
        ok = bool(ok)
        self.results.append((name, ok, detail))
        print(("PASS  " if ok else "FAIL  ") + name + (f"  -- {detail}" if detail and not ok else ""), flush=True)
        if not ok:
            self.failures += 1
        return ok

    def skip(self, name, why):
        self.results.append((name, None, why))
        print(f"SKIP  {name}  -- {why}", flush=True)

    def done(self):
        print(f"\n{len([r for r in self.results if r[1]])} passed, {self.failures} failed, "
              f"{len([r for r in self.results if r[1] is None])} skipped", flush=True)
        return self.failures
