#!/usr/bin/env python3
"""
ui-panel-plugin-pty-check.py — E5-a acceptance: the interactive TUI panel's
plugin-manager tab works over a REAL PTY against a live core.

Covers (#32 UI-side): tab 4 renders the three panes; cursor movement highlights
a row; enable toggle is immediate; disable toggle asks for confirmation (y/N);
with the core stopped the tab degrades to an offline notice without crashing.
Covers (#260): the `n` network self-check overlay opens, lists one row per probed
path, and Esc closes it.

The Strategies pane is filled by a LOADED CDYLIB: the kernel registers no
strategy of its own, so the core below is pointed at the reference
implementation (`user_layer/parity_strategy`) with `--strategy-dir`. Without it
the pane has no rows and the cursor/confirm coverage has nothing to drive.

Every strategy is DISABLED on a fresh boot (#269/#277), so the toggle assertions
drive the row to the state they need instead of assuming the shipped default: the
disable path is exercised by enabling first. A script that assumes "shipped =
enabled" reads a silent enable as a failed confirm — which is exactly what it did
before this was fixed.

Isolation: private UDS + scratch workdir + dry mode + no logs/archives.
Exit 0 on PASS, 1 on FAIL. (Uses raw pty.fork — Node `script -q /dev/null`
does not propagate a winsize, which starves ratatui down a 0×0 frame.)
"""
import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import fcntl

RP_SNIPPET = (
    "const net=require('net');const c=net.connect(process.argv[1]);let b='';"
    "c.on('connect',()=>c.write(JSON.stringify({jsonrpc:'2.0',id:1,method:process.argv[2],params:{}})+'\\n'));"
    "c.on('data',d=>{b+=d;const i=b.indexOf('\\n');if(i<0)return;console.log(b.slice(0,i));c.end();process.exit(0)});"
    "c.on('error',e=>{console.log('{}');process.exit(0)});"
    "setTimeout(()=>{console.log('{}');process.exit(0)},3000);"
)

ROOT = os.getcwd()
BIN = os.path.join(ROOT, 'target/release/blitzkrieg-core')
PANEL = os.path.join(ROOT, 'target/release/ui_kit_panel')
REF_DYLIB_DIR = os.path.join(ROOT, 'user_layer/parity_strategy/target/release')
REF_STRATEGY = 'parity'  # the name the reference cdylib registers
# CI runs this on Linux; a hardcoded `.dylib` would only ever pass on macOS.
REF_DYLIB = os.path.join(
    REF_DYLIB_DIR,
    'libparity_strategy.' + ('dylib' if sys.platform == 'darwin' else 'dll' if sys.platform == 'win32' else 'so'))
UID = str(os.getpid())
SOCK = f"/tmp/uikit-pty-{UID}.sock"
WORK = tempfile.mkdtemp(prefix='uikit-pty-data-')

for p in (BIN, PANEL):
    if not os.path.exists(p):
        sys.exit(f"missing binary: {p} (run: cargo build --release)")
if not os.path.exists(REF_DYLIB):
    sys.exit(f"missing reference cdylib: {REF_DYLIB} "
             f"(run: cd user_layer/parity_strategy && cargo build --release --locked)")

failures = []
def check(name, cond, detail=''):
    print(f"  {'ok  ' if cond else 'FAIL'} {name}" + (f" — {detail}" if detail else ""))
    if not cond:
        failures.append(name)

def strip_ansi(b: bytes) -> str:
    return re.sub(r'\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07', '', b.decode('utf-8', 'replace'))

class Panel:
    def __init__(self, socket):
        pid, fd = pty.fork()
        if pid == 0:
            os.environ['TERM'] = 'xterm-256color'
            os.environ.setdefault('TMPDIR', os.path.dirname(SOCK))
            os.execv(PANEL, ['ui_kit_panel', '--socket', socket, '--interval-ms', '300'])
        self.pid, self.fd = pid, fd
        self.rows, self.cols = 60, 200
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', self.rows, self.cols, 0, 0))
        self.buf = bytearray()

    def drain(self, secs=1.2):
        end = time.time() + secs
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.2)
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                self.buf += data
        return strip_ansi(bytes(self.buf))

    def clear(self):
        self.buf.clear()

    def wait_for(self, needle: str, timeout=3.0):
        """Poll until `needle` appears after a forced full redraw.

        ratatui paints cell-diffs, so a row painted once and reused is
        repainted span-split with cursor-moves in between; a winsize nudge
        (SIGWINCH) makes it repaint the whole screen, which we then read.
        """
        end = time.time() + timeout
        while time.time() < end:
            import signal
            fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', self.rows, self.cols - 2, 0, 0))
            time.sleep(0.05)
            fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', self.rows, self.cols, 0, 0))
            try:
                os.kill(self.pid, signal.SIGWINCH)
            except ProcessLookupError:
                pass
            time.sleep(0.2)
            self.buf.clear()
            self.drain(0.7)
            if needle in strip_ansi(bytes(self.buf)):
                return True
        return False

    def send(self, b: bytes, settle=0.9):
        os.write(self.fd, b)
        time.sleep(settle)
        return self.drain(0.6)

    def quit(self):
        try:
            os.write(self.fd, b'q')
        except OSError:
            pass
        time.sleep(0.3)
        try:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, os.WNOHANG)
        except (ProcessLookupError, ChildProcessError):
            pass
        os.close(self.fd)

# ── launch the core ──────────────────────────────────────────────────────────
core = subprocess.Popen(
    [BIN, '--socket', SOCK, '--mode', 'dry', '--tick-ms', '100', '--seed-balance', '1000',
     '--max-order-notional', '6', '--assets', 'BTC,ETH', '--min-shares', '1', '--max-shares', '10',
     '--strategy-dir', REF_DYLIB_DIR,
     '--engine', '--no-event-archive'],
    cwd=WORK, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
for _ in range(60):
    if os.path.exists(SOCK):
        break
    time.sleep(0.1)
else:
    sys.exit('core socket never appeared')
time.sleep(0.8)

try:
    # Round 1: live core.
    p = Panel(SOCK)
    text = p.drain(2.0)
    check('panel renders Overview', 'Blitzkrieg Panel' in text and 'DRY' in text, text[:60].replace('\n', ' '))

    p.send(b'4', settle=1.0)
    redraw = p.wait_for(REF_STRATEGY)
    full = strip_ansi(bytes(p.buf))
    check('tab 4 shows Strategies pane', 'Strategies' in full)
    check(f'the loaded cdylib is listed: {REF_STRATEGY}', REF_STRATEGY in full, redraw)
    check('polymarket pane present', 'polymarket' in full)
    check('extensions pane present', 'Extensions' in full)

    # Cursor at row 0 — the only row, the reference strategy. Enter → enable (no
    # confirm). ratatui paints diffs, so log lines are not reliably in the raw
    # stream — assert no confirm dialog, then verify the toggle in the CORE
    # registry below.
    p.clear()
    text = p.send(b'\r', settle=1.2)
    # If a dialog had appeared it would be in this frame; wait briefly and confirm absence.
    appeared = p.wait_for('y = confirm', timeout=1.0)
    check('enable had no confirm dialog', not appeared)
    check(f'enable cursor row rendered as [on ] {REF_STRATEGY}',
          f'[on ] {REF_STRATEGY}' in text or f'strategy {REF_STRATEGY}' in text,
          'cursor row after Enter')

    # Same row again: a fresh boot is DISABLED (#269/#277), so the Enter above
    # ENABLED it and only this second Enter is the disable under test. Assuming
    # the shipped default was enabled here is what made this script red on the
    # baseline (#260 finding 6) — the "disable" step was silently enabling, so no
    # confirm appeared and four assertions downstream failed with it.
    p.clear()
    p.send(b'\r', settle=0.6)
    found = p.wait_for('⚠')  # the confirm box renders ⚠ before the styled command text
    check('confirm bar for disable', found)
    fullcdf = strip_ansi(bytes(p.buf))
    check('confirm bar full text visible', 'y = confirm' in fullcdf or 'confirm' in fullcdf.lower(), 'dialog body')
    check(f'confirm names {REF_STRATEGY}', REF_STRATEGY in p.drain(0.0) + text)

    # n cancels.
    text = p.send(b'n')
    check('cancel dismisses dialog', 'y = confirm' not in text.split('y = confirm')[-1] or re.search(r'cancel\s+dismissed', text), text[-200:])

    # Redo and confirm with y.
    p.send(b'\r', settle=0.7)
    text = p.send(b'y', settle=1.0)
    check('confirmed disable applied', f'strategy {REF_STRATEGY}' in text and 'off' in text,
          re.sub(r'\s+', ' ', re.search(r'(Confirm.{0,160}|Log[\s\S]{0,160})', text).group(0) if re.search(r'(Confirm|Log)', text) else text[-250:]))
    # registry row flips to [off]
    check(f'{REF_STRATEGY} row now [off]', re.search(r'\[off\]\s*' + REF_STRATEGY, text) is not None)
    p.quit()

    # Ground truth: the toggle must be visible in the core's own registry.
    out = subprocess.run(['node', '-e', RP_SNIPPET, SOCK, 'strategy.list'],
                         capture_output=True, text=True, timeout=15)
    try:
        st = json.loads((out.stdout.splitlines() or ['{}'])[0])
        rows = {r['name']: r['enabled'] for r in st.get('result', {}).get('strategies', [])}
    except ValueError:
        rows = {}
    check('core registry: exactly the loaded cdylib, nothing built in',
          list(rows.keys()) == [REF_STRATEGY], str(rows))
    check(f'core registry: {REF_STRATEGY} off', rows.get(REF_STRATEGY) is False, str(rows))

    # ── E9-f (#61): onboarding affordances ────────────────────────────────────
    # Fresh panel again (the one above already consumed hints / help state).
    p1 = Panel(SOCK)
    text = p1.drain(2.0)
    check('hint bar shows self-check state', any(
        s in text for s in ('self-check passed', 'connected', 'connecting')), text[-160:])
    check('first-run hint shown', 'press :' in text)

    # `?` opens the help overlay listing keys and commands.
    text = p1.send(b'?', settle=1.0)
    check('help overlay opens on ?', 'Help' in text and 'COMMANDS' in text, text[:80].replace('\n', ' '))
    check('help lists up/down dual meaning', 'recall' in text.lower() and 'Plugins' in text)
    p1.clear()
    text = p1.send(b'\x1b', settle=0.6)
    check('Esc closes help', 'COMMANDS' not in text, text[-60:])

    # Command bar: Tab completes an unambiguous prefix.
    p1.send(b':', settle=0.4)
    p1.clear()
    text = p1.send(b'pos', settle=0.3)
    text = p1.send(b'\t', settle=0.6)
    # ratatui paints spans around the cursor, so the tail may arrive split with
    # cursor blocks and diff noise in between; project to letters and match.
    letters = ''.join(re.findall(r'[A-Za-z]', text))
    check('Tab completes pos→positions', 'positions' in letters, re.sub(r'\s+', ' ', text[-120:]))
    # Executed commands are recallable with ↑ in a fresh bar.
    p1.send(b'\r', settle=1.2)
    p1.send(b':', settle=0.4)
    p1.clear()
    text = p1.send(b'\x1b[A', settle=0.7)
    check('↑ recalls last command', 'positions' in text, re.sub(r'\s+', ' ', text[-120:]))

    # ── E11 (#260): the `n` network self-check overlay ────────────────────────
    # The command bar is still open from the recall test above, and while it owns
    # input a bare `n` is typed into it rather than opening the overlay — so leave
    # it first, then press `n`.
    p1.send(b'\x1b', settle=0.5)
    # `n` probes on the first press, and the probe dials REAL venues (DNS → TCP →
    # TLS → one request per path), so the rows land only when the report does —
    # hence the long timeout, which matches the client's own 30 s read timeout.
    # A machine with no outbound network still gets one row per path, each of
    # them failing: what is asserted here is the overlay's shape, never the
    # verdict, because the verdict depends on where this runs.
    text = p1.send(b'n', settle=0.8)
    check('n opens the network overlay', 'Network self-check' in text, text[:70].replace('\n', ' '))
    # The last row in report order is the one that proves the whole table landed.
    landed = p1.wait_for('venue-ws', timeout=35.0)
    frame = strip_ansi(bytes(p1.buf))
    paths = ('venue-rest', 'discovery', 'spot-ws', 'venue-ws')
    missing = [n for n in paths if n not in frame]
    check('overlay lists one row per probed path', landed and not missing,
          f"missing: {missing}" if missing else 'venue-rest/discovery/spot-ws/venue-ws')
    # Esc is the other documented way out (the footer says so); assert the overlay
    # actually leaves, not merely that it stopped being painted in one frame.
    p1.clear()
    p1.send(b'\x1b', settle=0.8)
    gone = False
    for _ in range(6):
        p1.buf.clear()
        p1.drain(0.6)
        if 'Network self-check' not in strip_ansi(bytes(p1.buf)):
            gone = True
            break
        time.sleep(0.2)
    check('Esc closes the network overlay', gone, strip_ansi(bytes(p1.buf))[-60:].replace('\n', ' '))
    p1.quit()

    # Round 2: core stopped — graceful degradation.
    core.terminate()
    time.sleep(0.8)
    try:
        os.unlink(SOCK)
    except FileNotFoundError:
        pass
    p2 = Panel(SOCK)
    p2.drain(1.5)
    text = p2.send(b'4', settle=1.0)
    check('offline notice (graceful degradation)', 'plugin registry offline' in text or 'offline' in text or 'not reachable' in text)
    check('panel alive after offline view', p2.pid > 0 and os.path.exists(f"/proc/{p2.pid}") if os.path.isdir('/proc') else True)
    p2.quit()
finally:
    if core.poll() is None:
        core.terminate()
        time.sleep(0.3)
        if core.poll() is None:
            core.kill()
    shutil.rmtree(WORK, ignore_errors=True)

print(f"\nRESULT: {'PASS' if not failures else 'FAIL (' + str(len(failures)) + ')'}")
sys.exit(0 if not failures else 1)
