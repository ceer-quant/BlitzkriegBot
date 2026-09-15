#!/usr/bin/env python3
"""PTY capture helper — runs a TUI binary on a real PTY and records the raw
output into a result file when a marker file is removed by the supervisor.

Usage: pty_capture.py <socket> <panel-bin> <marker> <result> [cols] [rows]
"""
import os, pty, select, struct, fcntl, termios, re, sys, signal

sock, panel, marker, result = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
cols = int(sys.argv[5]) if len(sys.argv) > 5 else 220
rows = int(sys.argv[6]) if len(sys.argv) > 6 else 70

pid, fd = pty.fork()
if pid == 0:
    os.environ['TERM'] = 'xterm-256color'
    os.execv(panel, ['ui_kit_panel', '--socket', sock, '--interval-ms', '60000'])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
allbuf = bytearray()
import os.path as P
import time
# Nudge the window size once so ratatui performs one full repaint before we
# start asserting screen text: diff rendering splits words across cursor moves.
def _nudge():
    for size in ((rows, cols), (rows, cols - 2), (rows, cols)):
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', *size, 0, 0))
        time.sleep(0.15)
_nudge()
while P.exists(marker):
    r, _, _ = select.select([fd], [], [], 0.3)
    if r:
        try:
            data = os.read(fd, 65536)
            if data:
                allbuf.extend(data)
        except OSError:
            break
try:
    os.write(fd, b'q')
except OSError:
    pass
try:
    os.kill(pid, signal.SIGKILL)
except ProcessLookupError:
    pass
text = allbuf.decode('utf-8', 'replace')
text = re.sub(r'\x1b\[[0-9;?]*[A-Za-z]', '', text)
open(result, 'w').write(text)
