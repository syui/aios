#!/usr/bin/env python3
# 対話する aish のテスト (test/aish.sh から): python3 test/aish-tty.py AISH PLUGINS
#   端末 (pty) につないでキーを打ち、出てくるもの (色のエスケープ) を見る: aish-highlight の色、Tab の補完
#   (aish-complete)、履歴の続きのグレー (aish-suggest) と → で決める、aish-time の終了コード、履歴のファイル。
#   aish -c と MCP のテストではためせない、人が打つときの顔。tmux なしで CI でも動く
import fcntl, os, pty, select, shutil, struct, sys, tempfile, termios, time

aish, plugins = sys.argv[1], sys.argv[2]
work = tempfile.mkdtemp()
home = work + '/home'
os.makedirs(home)
open(home + '/.aishrc', 'w').write('plugin aish-complete\nplugin aish-suggest\nplugin aish-highlight\nplugin aish-time\nPS1="ready$ "\n')
env = {'PATH': '/usr/bin:/bin', 'HOME': home, 'AISH_PLUGIN_PATH': plugins, 'TERM': 'xterm-256color', 'LANG': 'C.UTF-8'}
fails = []

pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 100, 0, 0))
    os.chdir(work)
    os.execve(aish, [aish], env)

buf = b''


def check(name, ok):
    if not ok:
        fails.append(name)
        print('FAIL', name, repr(buf[-300:]))


def expect(want, timeout=5):
    # want が出るまで読む (プラグインの答えは打ったあとに少しおくれて来る)
    global buf
    end = time.time() + timeout
    while want.encode() not in buf:
        left = end - time.time()
        if left <= 0 or not select.select([fd], [], [], left)[0]:
            return False
        try:
            data = os.read(fd, 4096)
        except OSError:
            return False
        if not data:
            return False
        buf += data
    return True


def key(s):
    # 前に出たものは見ない (この打ったあとに出たものだけ)
    global buf
    buf = b''
    os.write(fd, s.encode())


check('prompt', expect('ready$ '))
# aish-highlight: ないコマンドは太い赤
key('ech')
check('highlight unknown command', expect('\x1b[1;31mech'))
# aish-complete: Tab で ech → echo (あるコマンドは緑)
key('\t')
check('tab completes', expect('\x1b[32mecho'))
key('\x15echo hello-aish\r')
check('run a line', expect('\r\nhello-aish\r\n') and expect('ready$ '))
# aish-suggest: 打っている行に続く履歴をグレーで、→ で決める
key('echo he')
check('suggest from history', expect('\x1b[90mllo-aish'))
key('\x1b[C\r')
check('right arrow takes the suggestion', expect('\r\nhello-aish\r\n') and expect('ready$ '))
# aish-time: しくじったときの終了コード
key('false\r')
check('time shows exit', expect('\x1b[31mexit 1') and expect('ready$ '))
# exit だけなら前のコマンド (false) の 1 で終わる (bash と同じ)
key('exit 0\r')
_, st = os.waitpid(pid, 0)
check('exit', os.waitstatus_to_exitcode(st) == 0)
hist = open(home + '/.aish_history').read() if os.path.exists(home + '/.aish_history') else ''
check('history file', 'echo hello-aish' in hist and 'false' in hist)
shutil.rmtree(work, ignore_errors=True)
print('tty: %d failed%s' % (len(fails), (' (' + ', '.join(fails) + ')') if fails else ''))
sys.exit(1 if fails else 0)
