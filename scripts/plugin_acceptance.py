"""Isolated terminal acceptance; run after cargo build -p rness-cli."""
import fcntl
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time

BINARY = Path(__file__).resolve().parents[1] / "target/debug/rness"
SOURCE = """return function(opts,p)
p.action('insert',{scope='promptbox',description='Insert marker',run=function(ctx) ctx.promptbox.insert('ACCEPTANCE_MARKER') end})
p.keys({insert={action='insert',key='<F6>'}})
end
"""


def run(source, override, key, expected, core_remap=False):
    home = Path(tempfile.mkdtemp(prefix="rness-acceptance-"))
    root = home / ".rness"
    root.mkdir()
    (root / "review.lua").write_text(SOURCE)
    if source == "inline":
        declaration = "config=" + SOURCE.removeprefix("return ")
    elif source == "file":
        declaration = "file='./review.lua'"
    else:
        package = home / "review-package"
        package.mkdir()
        (package / "plugin.lua").write_text(SOURCE)
        (package / "rness-plugin.json").write_text('{"name":"review","entrypoint":"plugin.lua","api_version":1}')
        (root / "packages").mkdir()
        import json
        (root / "packages/lock.json").write_text(json.dumps({"review": {"source": str(package), "revision": None, "directory": str(package)}}))
        declaration = "package='review'"
    identity = "" if source == "package" else "name='review',"
    config = "rness.plugins.setup({{" + identity + declaration + ",keys=" + override + "}})"
    if core_remap:
        config += """
rness.keymap.setup({
 {scope='promptbox',key='enter',action='core.promptbox.noop'},
 {scope='promptbox',key='f8',action='core.promptbox.submit'}
})
rness.commands.register{name='acceptance',run=function()
 local f=assert(io.open('core-command-ran','w')); f:write('CORE_REMAP_OK'); f:close()
 return 'CORE_REMAP_OK'
end}
"""
    (root / "init.lua").write_text(config)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 110, 0, 0))
    env = {k: v for k, v in os.environ.items() if k in ("PATH", "LANG", "LC_ALL", "TMPDIR")}
    env.update(HOME=str(home), TERM="xterm-256color", XDG_CONFIG_HOME=str(home / ".config"))
    process = subprocess.Popen([str(BINARY), "--route", "test=http://127.0.0.1:1/v1,none", "-m", "test/model", "--instructions", "none"], cwd=home, env=env, stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    output = bytearray()

    def collect(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([master], [], [], max(0, end - time.monotonic()))
            if ready:
                try:
                    output.extend(os.read(master, 65536))
                except OSError:
                    break

    try:
        collect(3)
        assert process.poll() is None, "startup failed"
        if core_remap:
            os.write(master, b"/acceptance\x1b")
            collect(1)
            os.write(master, b"\r")
            collect(1)
            assert not (home / "core-command-ran").exists(), "disabled Enter submitted"
            os.write(master, b"\x1b[19~")
            collect(2)
            assert (home / "core-command-ran").read_text() == "CORE_REMAP_OK", "F8 did not invoke core submit"
            os.write(master, b"/help bindings\x1b")
            collect(1)
            os.write(master, b"\x1b[19~")
            collect(2)
            assert b"core.promptbox.noop" in output and b"core.promptbox.submit" in output
        else:
            os.write(master, key)
            collect(2)
            assert (b"ACCEPTANCE_MARKER" in output) == expected, (source, override, home)
        os.write(master, b"\x03")
        collect(1)
        process.wait(timeout=5)
        assert process.returncode == 0
        print("PASS", source, override, home)
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        (home / "terminal.raw").write_bytes(output)
        os.close(master)


if __name__ == "__main__":
    for source in ("inline", "file", "package"):
        for override, key, expected in (("{}", b"\x1b[17~", True), ("{insert='<F8>'}", b"\x1b[19~", True), ("{insert='<F8>'}", b"\x1b[17~", False), ("false", b"\x1b[17~", False)):
            run(source, override, key, expected)
        run(source, "false", b"", False, core_remap=True)
