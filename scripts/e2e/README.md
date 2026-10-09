# scripts/e2e — shared E2E / perf infrastructure (WP-0)

Stdlib-only Python 3. Every script runs the **frozen** binaries; nothing here builds rness.

| File | Purpose |
|---|---|
| `../fake_provider.py` | Fake model provider: Anthropic / OpenAI chat / ChatGPT Responses wire shapes, fault scenarios |
| `harness.py` | `with_rness()` (headless / TUI-in-tmux / `--serve`), tmp HOME, process + tmux helpers |
| `fixtures/gen_session.py` | Synthetic session-log generator and real-corpus sampler (copies, read-only source) |
| `perf.py`, `compare_baseline.py` | JSON-lines perf reporter, baseline comparison (>25 % median = regression) |
| `selftest.py` | Every scenario × shape through headless rness, plus the harness modes (≈280 checks, ~2 min) |
| `cli_e2e.py` | WP-7 CLI matrix, `--list` timing, headless exit codes, SIGINT, control socket, install upgrade |
| `baselines/<probe>.json` | Stored perf medians |

## Conventions (§0)

- Binaries: `~/.cache/rness-e2e/bin/rness` (release) and `rness-debug`; override with `RNESS_E2E_BIN` / `RNESS_E2E_BIN_DEBUG`.
- WP *N* owns ports `87N0–87N9` (`harness.wp_port(N, slot)`, `free_wp_port(N)`), tmux sessions `rness-e2e-wpN-*`
  (`Tmux` asserts the prefix), and tmp dirs `$TMPDIR/rness-e2e-wpN-*` (`tmp_dir(N, tag)`).
- HOME is always a tmp dir, with the default flavor copied in the same way `install.sh` copies it. Real `~/.rness` is never written to.
- Check output is `PASS|FAIL|SKIP|XFAIL|XPASS name -- detail`, and the exit code is the number of FAILs (`harness.Checks`).

## Fake provider

```python
import sys; sys.path.insert(0, "scripts")            # harness does this for you
from fake_provider import FakeProvider
with FakeProvider(port=0, stall_seconds=3, record="/tmp/x/requests") as fp:
    fp.port; fp.anthropic_url("echo"); fp.openai_url("cut"); fp.responses_url("ok")
    fp.set_script("demo", [{"tool": "Bash", "args": {"command": "ls"}}, {"text": "done {n}"}])
    fp.requests            # every request: seq, key, scenario, args, aux, n, bytes, summary{messages,tools,...}
    fp.main_requests("echo/t1"); fp.set_stall_seconds(20); fp.reset()
```
CLI: `python3 scripts/fake_provider.py --port 8700 [--shape auto|anthropic|openai|responses] [--record DIR] [--script-dir DIR] [--stall-seconds S] [--no-aux]`. Run `--help` for the scenario list.

**URL** = `http://127.0.0.1:PORT/<scenario>[:arg[:arg]][/free/form]` + the suffix that rness appends.
The whole prefix is the **counter key**, so use `/<scenario>/<your-test-id>` to get a private counter for
stateful scenarios. Title and compaction requests (`aux`) get a canned answer and **do not** advance counters.

| Scenario | Behaviour |
|---|---|
| `ok` (default) | text `Partial response from fake provider.` |
| `plan` | `exit_plan_mode` call, then text |
| `recover` | first request per key → 401, then ok |
| `heartbeat` | text, 2 s of `: keepalive` comments, then completes |
| `cut` / `disconnect` / `malformed` | stream closed early / socket closed before headers / bad JSON event |
| `stall` / `stall-headers` | silent for `stall_seconds` mid-stream / before headers |
| `stream-error`, `overloaded` | in-stream `overloaded_error` after the first delta |
| `http-NNN[:K]` | HTTP NNN (529 = overloaded); with K, only the first K requests fail |
| `retry-after-seconds[:S[:K]]`, `retry-after-date[:S[:K]]` | 429 + `Retry-After` (seconds or HTTP-date), then ok |
| `context-overflow[:K]` | HTTP 400 `prompt is too long` / `context_length_exceeded` |
| `huge-tool-json[:MB[:TOOL]]` | tool call whose args are MB (default 4) MiB of partial JSON |
| `slow-drip[:MS]` | the whole body at 1 byte per MS (default 50) ms |
| `split-utf8` | multi-byte characters split across TCP writes |
| `dup-index` | two blocks with the same index (OpenAI: two tool calls at index 0) |
| `unknown-event` | unknown events, pings and unknown deltas interleaved |
| `tool-loop[:N[:TOOL]]` | calls TOOL (default `X`, an unknown tool) until N results follow the prompt, then end_turn |
| `echo` | replies `ECHO {json}` with messages, user_messages, tool_results, tools, tool_names, bytes, system_bytes, last_user, n |
| `script[:NAME]` | step *i* answers main request *i*. A step is `"text"` or `{text, thinking, tool, args, tools:[{name,args,id}], fault, delay_ms, scenario}`. Steps can be a list or `{"steps": [...], "then": "repeat-last" or "ok"}` |

## Running rness

```python
sys.path.insert(0, "scripts/e2e"); from harness import *
with with_rness(wp=3, scenario="echo") as r:               # mode="headless" (default)
    res = r.headless("hi")                                 # Result: rc, stdout, stderr, session, elapsed, attempts
    r.headless("more", session=res.session, scenario="cut/t2")
    r.log(res.session)                                     # list of envelope dicts; r.root = $HOME/.rness/sessions
    p = r.spawn_headless("hi", scenario="stall/x")         # Popen (out/err in p.out_path/p.err_path)
    r.run("--list")                                        # arbitrary args (provider flags + --instructions none added)
with with_rness(wp=3, mode="tui", tag="resize") as r:      # tmux session rness-e2e-wp3-resize
    r.tmux.say("hello"); r.tmux.wait_for(r"Partial response", 20); r.tmux.keys("C-c"); r.pid
with with_rness(wp=3, mode="serve") as r:                  # --serve 127.0.0.1:87N?
    sid = r.api("POST", "/api/sessions", {"workspace": str(r.work)})["session"]
    r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": "followup",
                                   "content": [{"kind": "text", "text": "hi"}]})
    for ev, data in r.sse(f"/api/events/{sid}"): ...
```
`with_rness` keyword arguments: `shape` (`anthropic|openai|responses`), `provider=` (share one FakeProvider), `debug=True`
(rness-debug), `binary=`, `args=` (extra CLI args), `env=`, `init_append=` (Lua appended to init.lua), `flavor=False`,
`allow_generic_agents=True`, `record=True` (bodies go to `<base>/requests`), `keep=True` (keep the tmp dir), `tui_wait=` (regex, or `None`).
Anything that isn't a `with_rness` keyword goes to FakeProvider, for example `stall_seconds=`.
Shape wiring (see `provider_flags`):
- anthropic: `-m anthropic/fake --base-url URL` with `ANTHROPIC_API_KEY=fake`
- openai: `--route fake=URL/v1,none -m fake/fake`
- responses: `-m chatgpt/fake --base-url URL`, with a fake OAuth `credentials.json` written to the tmp HOME

Helpers: `rss_kb`, `footprint_kb` (macOS phys_footprint), `cpu_percent`, `fd_count` (lsof), `children`,
`process_tree`, `descendants`, `alive` (zombies count as dead), `kill9`, `kill_tree`, `kill9_when(pid, pred)`,
`log_grew_to(path, regex)`, `wait_until(pred, timeout)`, `Sampler(pid, every=1.0, fds=True, footprint=False)` with
`.samples` and `.peak()`, `Timer` with `.ms`, `Tmux(name)` with `.start/.capture/.wait_for/.wait_stable/.keys/.type/.say/.resize/.pane_pid/.kill`,
`read_log/event_types/list_session_dirs`, and `make_home`.

## Fixtures

```python
from fixtures.gen_session import generate, sample_real
ids = generate(root, workspace, turns=50, tools_per_turn=2, tool_result_bytes=8192, compactions=2,
               prunes=3, forks=1, attempts=1, sessions=1, seed=7)    # roots first, then forks
ids = sample_real(dest_root, n=5, workspace=cwd)  # COPIES the N largest ~/.rness/sessions (+ fork parents)
```
The `workspace` argument must be the cwd that rness runs in (`r.work`), or `--list` will not show the sessions. Pass `root=r.root`.
`sample_real` with `workspace=` rewrites the header in the copy only. CLI: `gen_session.py gen ROOT --workspace DIR --turns N ...` / `gen_session.py real ROOT --n 5`.
The generated compaction/prune/fork structure matches what the engine writes (started → summary{replaces} → finished{started}; prune{replaces, result}; header.parent{session, at}).

## Perf

```python
from perf import Reporter, compare_baseline, write_baseline, format_comparison
rep = Reporter("tui-scroll-50k", wp=3, binary=BIN)        # → $RNESS_E2E_PERF_OUT or $TMPDIR/rness-e2e-perf/<probe>.jsonl
rep.sample("frame_ms", 12.5, unit="ms", session_events=50000)
res = compare_baseline("tui-scroll-50k", rep.medians())   # {"ok", "missing", "rows":[{metric,current,baseline,ratio,status}]}
print(format_comparison(res)); rep.close()
```
Lower is always better, so record throughput as time per unit. The CLI is `compare_baseline.py RESULTS.jsonl... [--threshold 0.25] [--commit SHA] [--write]`, and its exit code is the number of regressed probes.

## Gotchas
- `rness -p` **exits 0 even when the turn fails**. Check `turn/ended.outcome` in the log (`r.log(sid)`), not `rc`.
- `--list` (and anything else that doesn't start a turn) still needs `-m`. `r.run` adds it for you.
- The idle timeout counts complete SSE events or comments. A slow byte trickle is not progress, so set a short timeout with
  `init_append="rness.providers.set_stream_idle_timeout('anthropic'|'fake'|'chatgpt', ms)"`. The default is 300 s.
- Retries: 3 attempts in total, with 0.5 s / 1 s backoff unless the response has `Retry-After`. Each retry advances the scenario counter.
- `context-overflow` triggers compaction only when there is enough history beyond `retain_tokens` (24 k tokens). A fresh session just fails.
- OpenAI and Responses streams that end early, and Responses streams with malformed events, are **committed as complete** (known bugs). Plan around that.
- `kill -9` of a `--control-socket` TUI leaves the socket file behind. Unlink it before you restart on the same path.
