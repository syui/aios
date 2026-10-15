#!/usr/bin/env python3
# aios を QEMU で起こしたままにして、外からコマンドを送る (人の手の代わり。Claude が aios の中で確かめるため)
#
#   test/vm.py start [--www DIR]   起こして、プロンプトが出るまで待つ (bin/run.sh。画面は AIOS_DISPLAY=none。
#                                  AIOS_KERNEL=FILE でそのカーネルを。ほかの AIOS_* も run.sh へ)
#   test/vm.py run 'CMD' [-t SEC]  シリアルの端末で動かし、出力を出して、同じ終わりの番号で終わる (何行でもよい)
#   test/vm.py bg 'CMD'            中でうしろで動かしてすぐ答える ({"job": ID})。長いもの (ap -S firefox など) のあいだも run が使える
#   test/vm.py job [ID] [-w SEC] [--kill]
#                                  うしろのものの様子と、前に見せたところから先の出力 (-w 秒まで終わるのを待つ。ID がなければ一覧)
#   test/vm.py type 'TEXT'         画面のキーボードで打つ (\n で Enter。aiwm の上の aiterm など)
#   test/vm.py keys ARG...         画面のキーボードで: @ で始まるものは QEMU のキーの名前 (@alt-b @ctrl-l @ret @f5)、
#                                  ほかは文字として打つ (vm.py keys @ctrl-l 'example.com' @ret)
#   test/vm.py shot FILE.png       画面を撮る (png は ImageMagick の convert があれば。なければ .ppm)
#   test/vm.py put LOCAL REMOTE    ファイルを中へ (16 KB までは gzip と base64 でシリアルから、大きいものは http と fetch で)
#   test/vm.py get REMOTE LOCAL    中のファイルを外へ (gzip と base64 を 1 行にしてシリアルで。sha256 を中と外でくらべる。16 MB まで)
#   test/vm.py reboot [-t SEC] [--reset-after SEC] [--choose TEXT]
#                                  中で sudo reboot して、またプロンプトまで待つ。--reset-after の秒で来なければ
#                                  QEMU をリセットする (止まった新しいカーネルの電源を入れなおすかわり)
#   test/vm.py mon 'CMD'           QEMU のモニタへ
#   test/vm.py log [N]             シリアルの終わりの N 行 (既定 40)
#   test/vm.py status / stop
#
# 起こしたものは、うしろの小さなサーバー (serve) が持つ。シリアルはいつも読みつづけるので、カーネルの
# 出力でつまらない。--www DIR で DIR を http://10.0.2.2:8000/ に出す (aios の中から fetch で取れる)。
# 置き場は $AIOS_VM_DIR (既定 /tmp/aios-vm-UID): ctl (サーバーの口)、mon (QEMU のモニタ)、serial.log
import json, os, pty, re, select, shutil, signal, socket, subprocess, sys, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.environ.get('AIOS_VM_DIR') or '/tmp/aios-vm-%d' % os.getuid()
CTL, MON, LOG = DIR + '/ctl', DIR + '/mon', DIR + '/serial.log'
PROMPT = re.compile(rb'aios[^\n]*\][$#] ')
# 起動のとちゅうでカーネルが止まった (それより待ってもプロンプトは来ない)
PANIC = re.compile(rb'(^|\n)panic: ')
# エスケープ: CSI (色、カーソル)、OSC (ESC ] ... BEL か ESC \\。zig の進みぐあい ESC ]9;4 など)、ESC と 1 文字 (ESC M など)
ESC = re.compile(r'\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()][0-9A-Za-z]|\x1b[@-Z\\-_78=>]')
ANSI = re.compile(ESC.pattern + r'|\r')
KEYS = {'\\': 'backslash', ' ': 'spc', '-': 'minus', '.': 'dot', '/': 'slash', '\n': 'ret', '=': 'equal', ';': 'semicolon', ',': 'comma',
        "'": 'apostrophe', '`': 'grave_accent', '[': 'bracket_left', ']': 'bracket_right', '\t': 'tab',
        '_': 'shift-minus', '>': 'shift-dot', '<': 'shift-comma', '|': 'shift-backslash', '&': 'shift-7', '$': 'shift-4', '"': 'shift-apostrophe',
        '*': 'shift-8', '(': 'shift-9', ')': 'shift-0', ':': 'shift-semicolon', '~': 'shift-grave_accent', '#': 'shift-3', '!': 'shift-1',
        '@': 'shift-2', '%': 'shift-5', '^': 'shift-6', '+': 'shift-equal', '?': 'shift-slash', '{': 'shift-bracket_left', '}': 'shift-bracket_right'}


def monitor(cmd, wait=0.5):
    c = socket.socket(socket.AF_UNIX)
    c.connect(MON)
    c.settimeout(5)
    time.sleep(0.2)
    try:
        c.recv(65536)
    except OSError:
        pass
    c.sendall((cmd + '\n').encode())
    time.sleep(wait)
    try:
        r = c.recv(65536)
    except OSError:
        r = b''
    c.close()
    # 答えは打った行のあとから、次の (qemu) の前まで
    t = r.decode('utf-8', 'replace').replace('\r', '')
    t = ANSI.sub('', t)
    return t.split('\n', 1)[-1].rsplit('(qemu)', 1)[0].strip()


class Serve:
    # bg のものの置き場 (中)
    JOBS = '/tmp/.vmjob'

    def __init__(self, www):
        self.buf = b''
        # bg で起こしたもの: ID → {cmd, t0, shown (ログのうち見せたバイト数)}
        self.jobs = {}
        self.cv = threading.Condition()
        self.log = open(LOG, 'wb')
        self.http = subprocess.Popen([sys.executable, '-m', 'http.server', '8000', '-d', www], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) if www else None
        # put の大きなファイル: DIR/xfer を http://10.0.2.2:8001/ に出して、中の fetch で取る (シリアルより速い)
        os.makedirs(DIR + '/xfer', exist_ok=True)
        self.xfer = subprocess.Popen([sys.executable, '-m', 'http.server', '8001', '--bind', '127.0.0.1', '-d', DIR + '/xfer'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        m, s = pty.openpty()
        env = dict(os.environ)
        env.setdefault('AIOS_SSH', '0')
        env.setdefault('AIOS_DISPLAY', 'none')
        env['AIOS_QEMU_ARGS'] = env.get('AIOS_QEMU_ARGS', '') + ' -monitor unix:%s,server,nowait' % MON
        self.q = subprocess.Popen([ROOT + '/bin/run.sh'] + ([os.environ['AIOS_KERNEL']] if os.environ.get('AIOS_KERNEL') else []), cwd=ROOT, stdin=s, stdout=s, stderr=s, env=env, close_fds=True, start_new_session=True)
        os.close(s)
        self.m = m
        threading.Thread(target=self.reader, daemon=True).start()

    def reader(self):
        while True:
            try:
                d = os.read(self.m, 65536)
            except OSError:
                d = b''
            if not d:
                with self.cv:
                    self.dead = True
                    self.cv.notify_all()
                return
            self.log.write(d)
            self.log.flush()
            # aish は端末に大きさを聞く (ESC [6n): 24 行目と答える
            if b'\x1b[6n' in d:
                os.write(self.m, b'\x1b[24;1R')
            with self.cv:
                self.buf += d
                self.last = time.time()
                self.cv.notify_all()

    dead = False
    last = 0.0

    def wait_quiet(self, quiet, timeout):
        # 出力が quiet 秒とだえるまで待つ (aish が行を描き終わり、打ったコマンドが動きだした)
        end = time.time() + timeout
        while time.time() < end:
            if time.time() - self.last >= quiet:
                return True
            time.sleep(0.1)
        return False

    def wait_for(self, pat, timeout):
        end = time.time() + timeout
        with self.cv:
            while True:
                mm = pat.search(self.buf)
                if mm:
                    out = self.buf[:mm.end()]
                    self.buf = self.buf[mm.end():]
                    return out, mm
                left = end - time.time()
                if left <= 0 or self.dead:
                    return None, None
                self.cv.wait(min(left, 1))

    def run(self, cmd, timeout):
        tag = 'VMEND%d' % int(time.time() * 1000)
        with self.cv:
            self.buf = b''
        # "cmd &; echo" and "cmd;; echo" are syntax errors: a line that ends
        # with & (into the background) or ; is followed by the echo as it is
        cmd = cmd.rstrip()
        sep = ' ' if cmd.endswith(';') or (cmd.endswith('&') and not cmd.endswith('&&')) else '; '
        os.write(self.m, (cmd + sep + 'echo ' + tag + ' $?\r').encode())
        out, mm = self.wait_for(re.compile((tag + r' (\d+)\r*\n').encode()), timeout)
        if out is None:
            # 止める (Ctrl-C) と、プロンプトに戻るのを少し待つ
            os.write(self.m, b'\x03')
            self.wait_for(PROMPT, 5)
            t = ESC.sub('', self.buf.decode('utf-8', 'replace'))
            i = t.rfind(tag + ' $?')
            if i < 0:
                # 打った行のこだまが終わっていない: VM がいそがしく、行を描きなおしているあいだに時間切れ。
                # Ctrl-C でその行は消えたので、コマンドは動いていない (描きなおしのくりかえしは返さない)
                return {'timeout': True, 'out': '', 'not_run': 'the VM was too busy to take the command line; it did not run (try again, or a longer timeout)'}
            return {'timeout': True, 'out': tidy(t[t.find('\n', i) + 1:] if '\n' in t[i:] else '')[-4000:]}
        # 色 (aish-highlight) を消してから、打った行のこだま (... echo TAG $?) のあとを。
        # aish は 1 文字ごとに行を描きなおすので、いちばん終わりのもの
        text = ESC.sub('', out[:mm.start()].decode('utf-8', 'replace'))
        i = text.rfind(tag + ' $?')
        if i >= 0:
            text = text[text.find('\n', i) + 1:] if '\n' in text[i:] else ''
        return {'status': int(mm.group(1)), 'out': tidy(text)}

    def put(self, path, b64, timeout=600, gz=False):
        # プロンプトで長い行を打つと、aish が 1 文字ごとに描きなおして遅いので、base64 -d を先に起こして、
        # その標準入力 (端末のふつうの行の入力) へ流しこむ。終わりは Ctrl-D
        import shlex
        tag = 'VMEND%d' % int(time.time() * 1000)
        with self.cv:
            self.buf = b''
        # gz: 縮めて送ったもの (シリアルは 1 秒に数十 KB なので)。gzip がなければ toybox の zcat で
        dec = 'base64 -d | { gzip -dc 2>/dev/null || toybox zcat; }' if gz else 'base64 -d'
        # 置き場のディレクトリがなければ作る (aish の write と同じ)。&& にしない: mkdir がしくじると base64 が
        # 起きず、流しこむ中身がプロンプトに打たれてしまう
        q = shlex.quote(path)
        os.write(self.m, ('mkdir -p "$(dirname %s)" 2>/dev/null; %s > %s; echo %s $?\r' % (q, dec, q, tag)).encode())
        # 行を描き終わって base64 が動きだす (出力がしずかになる) まで待つ。決まった時間だけ待つと、
        # 重いときに中身が行の編集のほうへ入ってしまう (長い行は画面の幅で切って描くので、こだまの文字では待てない)
        time.sleep(0.3)
        self.wait_quiet(1.0, 120)
        for i in range(0, len(b64), 4096):
            os.write(self.m, b64[i:i + 4096].replace('\n', '\r').encode())
        os.write(self.m, b'\x04')
        out, mm = self.wait_for(re.compile((tag + r' (\d+)\r*\n').encode()), timeout)
        if out is None:
            os.write(self.m, b'\x03')
            self.wait_for(PROMPT, 5)
            return {'timeout': True}
        return {'status': int(mm.group(1))}

    def bg(self, cmd):
        # うしろで動かす: 中の JOBS/ID.sh にして、サブシェルで . で読む (aish の書き方のまま)。出力は ID.log、
        # 終わりの番号は ID.rc (止められたらできない)、pid は ID.pid。aish はうしろのものを自分のグループに入れるので、
        # 止めるときは -pid に送れば孫まで止まる
        import base64
        jid = len(self.jobs) + 1
        f = '%s/%d' % (self.JOBS, jid)
        r = self.put(f + '.sh', base64.encodebytes(cmd.encode()).decode())
        if r.get('status') != 0:
            return {'error': 'could not write %s.sh in the VM' % f, 'put': r}
        # . で読むものはもう 1 つ内側のサブシェルで (その中の exit 3 が rc を書くところまで終わらせないように)
        r = self.run('rm -f {f}.rc {f}.log; ( ( . {f}.sh ); echo $? > {f}.rc ) > {f}.log 2>&1 < /dev/null & echo $! > {f}.pid'.format(f=f), 30)
        if r.get('status') != 0:
            return {'error': 'could not start it', 'out': r.get('out', '')}
        self.jobs[jid] = {'cmd': cmd, 't0': time.time(), 'shown': 0}
        return {'job': jid}

    def job(self, jid=None, wait=0, kill=False, limit=60000):
        # bg のものの様子: 終わったか (ID.rc があるか、pid がもういない)、前に見せたところから先の出力 (limit バイトまで)。
        # wait 秒まで終わるのを待つ。jid がなければ一覧
        if jid is None:
            return {'jobs': [{'id': i, 'cmd': j['cmd'][:120], 's': round(time.time() - j['t0'])} for i, j in self.jobs.items()]}
        j = self.jobs.get(jid)
        if j is None:
            return {'error': 'no job %s (vm job without an id lists them)' % jid}
        f = '%s/%d' % (self.JOBS, jid)
        if kill:
            self.run('p=$(cat {f}.pid 2>/dev/null); [ -n "$p" ] && {{ kill -TERM -$p 2>/dev/null || kill -TERM $p; }}'.format(f=f), 20)
        mark = '@@VMJOB@@'
        end = time.time() + wait
        while True:
            # 終わりの番号 (なければ空)、pid がいるか、ログの大きさ、まだ見せていないところ
            r = self.run(('cat {f}.rc 2>/dev/null; echo {m}; p=$(cat {f}.pid 2>/dev/null); [ -n "$p" ] && [ -d /proc/$p ] && echo alive; echo {m}; '
                          'wc -c < {f}.log 2>/dev/null; echo {m}; tail -c +{o} {f}.log 2>/dev/null | head -c {n}').format(f=f, m=mark, o=j['shown'] + 1, n=limit), 30)
            parts = r.get('out', '').split(mark)
            if len(parts) < 4:
                return {'error': 'could not read the job', 'out': r.get('out', '')}
            rc, alive, size, new = parts[0].strip(), 'alive' in parts[1], parts[2].strip(), parts[3]
            done = rc != '' or not alive
            if done or time.time() >= end:
                break
            time.sleep(1)
        size = int(size) if size.isdigit() else j['shown']
        got = min(size, j['shown'] + limit)
        res = {'id': jid, 'done': done, 'ms': int((time.time() - j['t0']) * 1000), 'out': new.strip('\n'), 'shown_before': j['shown']}
        j['shown'] = got
        if done:
            if rc.lstrip('-').isdigit():
                res['status'] = int(rc)
            else:
                res['status'] = None
                res['note'] = 'stopped before it ended (no exit status)'
        if got < size:
            res['more'] = size - got
        return res

    def reboot(self, timeout, reset_after=None, choose=None):
        # 中で sudo reboot して、またプロンプトまで待つ。reset_after 秒で来なければ QEMU をリセットする (新しいカーネルが
        # 止まったとき、人が電源を入れなおすかわり。aiboot は前のカーネルで起こしなおす)
        t0 = time.time()
        with self.cv:
            self.buf = b''
        os.write(self.m, b'sudo reboot\r')
        # 打った行のこだまもプロンプトに見えるので、まず init が止めにかかるのを待つ
        if self.wait_for(re.compile(rb'aios: (restart|stopping services)'), 60)[0] is None:
            return {'ready': False, 'error': 'the VM did not start to reboot (sudo reboot)'}
        # choose: aiboot の一覧 (loader.conf に timeout があるとき) で、題にこの文字のある項目の番号をシリアルから押す
        chose = None
        if choose:
            ok, mm = self.wait_for(re.compile(rb'boot \d+ in \d+ s'), min(timeout, 120))
            if mm is None:
                return {'ready': False, 'error': 'no aiboot menu (is there a timeout in /boot/loader/loader.conf?)'}
            # 一覧は wait_for が返した出力の中 (self.buf からは、見つけたところまでが消えている)
            menu = ANSI.sub('', ok.decode('utf-8', 'replace'))
            for m in re.finditer(r'^\s*\*?\s*(\d)\. (.*)$', menu, re.M):
                if choose in m.group(2):
                    chose = m.group(2).strip()
                    os.write(self.m, m.group(1).encode())
                    break
            else:
                return {'ready': False, 'error': 'no menu entry with %r' % choose, 'menu': menu[-800:]}
        pat = re.compile(PROMPT.pattern + b'|' + PANIC.pattern)
        reset = False
        while True:
            left = timeout - (time.time() - t0)
            if left <= 0:
                return {'ready': False, 'reset': reset, 'boot_s': round(time.time() - t0, 1), 'error': 'no prompt after the reboot (see log)'}
            wait = min(left, reset_after - (time.time() - t0)) if reset_after and not reset else left
            ok, mm = self.wait_for(pat, max(wait, 0.1))
            if mm is not None:
                if not PROMPT.search(mm.group(0)):
                    self.wait_quiet(1, 5)
                    return {'ready': False, 'panic': True, 'reset': reset, 'boot_s': round(time.time() - t0, 1)}
                break
            if reset_after and not reset and time.time() - t0 >= reset_after:
                monitor('system_reset')
                reset = True
        time.sleep(1)
        self.run('unset -f chpwd 2>/dev/null', 10)
        add_ca(self)
        r = {'ready': True, 'reset': reset, 'boot_s': round(time.time() - t0, 1)}
        if chose:
            r['chose'] = chose
        return r

    def type(self, text):
        c = socket.socket(socket.AF_UNIX)
        c.connect(MON)
        c.settimeout(2)
        time.sleep(0.2)
        for ch in text:
            k = KEYS.get(ch) or (('shift-' + ch.lower()) if ch.isupper() else ch)
            c.sendall(('sendkey %s\n' % k).encode())
            time.sleep(0.06)
        time.sleep(0.5)
        c.close()

    def keys(self, args):
        # @NAME は QEMU のキーの名前 (組み合わせも: alt-b)。ほかは文字として
        c = socket.socket(socket.AF_UNIX)
        c.connect(MON)
        c.settimeout(2)
        time.sleep(0.2)
        for a in args:
            names = [a[1:]] if a.startswith('@') else [KEYS.get(ch) or (('shift-' + ch.lower()) if ch.isupper() else ch) for ch in a]
            for k in names:
                c.sendall(('sendkey %s\n' % k).encode())
                time.sleep(0.08)
        time.sleep(0.5)
        c.close()

    def stop(self):
        try:
            os.write(self.m, b'\x03')
            time.sleep(1)
            os.write(self.m, b'sudo poweroff\r')
        except OSError:
            pass
        for _ in range(30):
            if self.q.poll() is not None:
                break
            time.sleep(1)
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(self.q.pid, sig)
            except ProcessLookupError:
                break
            time.sleep(2)
        if self.http:
            self.http.kill()
        self.xfer.kill()


def add_ca(vm):
    # クラウドのセッションでは、外への https を途中で開けて見るものがあり、aios の知らない CA で署名しなおす
    # (ap -Sy が invalid peer certificate: UnknownIssuer)。ホストの CA の束を中の ca-certificates.crt のうしろに足す
    # (aios のものは消さない。一度だけ)。AIOS_VM_CA=FILE でどの束か、AIOS_VM_CA=none でしない
    ca = os.environ.get('AIOS_VM_CA') or ('/root/.ccr/ca-bundle.crt' if os.path.exists('/root/.ccr/ca-bundle.crt') else '')
    if not ca or ca == 'none' or not os.path.exists(ca):
        return
    shutil.copy(ca, DIR + '/xfer/host-ca.crt')
    dst = '/etc/ssl/certs/ca-certificates.crt'
    vm.run("fetch http://10.0.2.2:8001/host-ca.crt -o /tmp/.host-ca.crt >/dev/null && "
           "sudo -n sh -c 'grep -q \"aios-vm: host CA\" %s || { echo \"# aios-vm: host CA\"; cat /tmp/.host-ca.crt; } >> %s'" % (dst, dst), 30)


def clean(t):
    return ANSI.sub('', t).strip('\n')


def screen(t):
    # 画面に見えるように: aish は 1 文字ごとに \r で行の頭に戻って描きなおすので、行ごとに最後に描いたものだけ
    # (clean だけだと \r を消して描きなおしがみな 1 行につながる)
    out = []
    for line in t.split('\n'):
        segs = [ANSI.sub('', s) for s in line.split('\r')]
        out.append(next((s for s in reversed(segs) if s.strip()), ''))
    return '\n'.join(out).strip('\n')


def tidy(t):
    # コマンドの出力を見えるとおりに: 行ごとに \r で描きなおした最後のもの。進みぐあいの表示 (zig cc など) が
    # カーソルを上げて同じ何行かを描きなおしつづけたものは、くり返しを 1 つにまとめる
    lines = []
    for line in ESC.sub('', t).split('\n'):
        lines.append(next((s for s in reversed(line.split('\r')) if s.strip()), ''))
    out, i = [], 0
    while i < len(lines):
        for k in range(1, 9):
            blk = out[-k:]
            if len(blk) < k or not any(x.strip() for x in blk):
                continue
            n = 0
            while lines[i + n * k:i + (n + 1) * k] == blk:
                n += 1
            # 1 回だけ同じなのはふつうの出力にもあるので、2 回から
            if n >= 2:
                out.append('... (the %d line%s above repeated %d more times)' % (k, 's' if k > 1 else '', n))
                i += n * k
                break
        else:
            out.append(lines[i])
            i += 1
    return '\n'.join(out).strip('\n')


def serve(www):
    os.makedirs(DIR, exist_ok=True)
    for f in (CTL, MON):
        try:
            os.unlink(f)
        except FileNotFoundError:
            pass
    vm = Serve(www)
    # 答えなくなったときに stop が止められるように
    open(DIR + '/pids', 'w').write('%d %d\n' % (os.getpid(), vm.q.pid))
    t0 = time.time()
    ok, mm = vm.wait_for(re.compile(PROMPT.pattern + b'|' + PANIC.pattern), 900)
    panicked = mm is not None and not PROMPT.search(mm.group(0))
    if panicked:
        # panic のあとの行 (わけ) もログに入るまで
        vm.wait_quiet(1, 5)
        ok = None
    # snapshot と www も答える (aish-pkg の pkg_test vm が、動いている VM をそのまま使ってよいか見る)
    ready = {'ready': ok is not None, 'boot_s': round(time.time() - t0, 1), 'snapshot': '-snapshot' in os.environ.get('AIOS_QEMU_ARGS', ''), 'www': www}
    if panicked:
        ready['panic'] = True
    if ok is None:
        ready['out'] = clean(open(LOG, 'rb').read()[-3000:].decode('utf-8', 'replace'))
    time.sleep(2)
    if ok is not None:
        # /etc/aishrc の「cd のあとの ls」(chpwd) は人のため。run の出力にまざらないように外す
        # (aish --mcp で外しているのと同じ)
        vm.run('unset -f chpwd 2>/dev/null', 10)
        add_ca(vm)
    srv = socket.socket(socket.AF_UNIX)
    srv.bind(CTL)
    srv.listen(4)
    while True:
        c, _ = srv.accept()
        f = c.makefile('rw')
        try:
            req = json.loads(f.readline())
            op = req.get('op')
            if op == 'status':
                r = dict(ready, running=vm.q.poll() is None)
            elif not ready['ready'] and op != 'stop':
                r = dict(ready, error='the kernel panicked while booting (see out)' if ready.get('panic') else 'aios did not reach the prompt (see log)')
            elif op == 'run':
                cmd = req['cmd']
                # 何行もあるもの: プロンプトに流すと続きを待って止まるので、ファイルにして . で読む (cd や変数は残る)
                if '\n' in cmd.strip('\n'):
                    import base64
                    r = vm.put('/tmp/.vm-run.sh', base64.encodebytes(cmd.encode()).decode())
                    cmd = '. /tmp/.vm-run.sh' if r.get('status') == 0 else cmd
                r = vm.run(cmd, req.get('timeout', 600))
            elif op == 'put':
                r = vm.put(req['path'], req['b64'], gz=req.get('gz', False))
            elif op == 'bg':
                r = vm.bg(req['cmd'])
            elif op == 'job':
                r = vm.job(req.get('id'), req.get('wait', 0), req.get('kill', False))
            elif op == 'fetch':
                # DIR/xfer/NAME (put が置いた大きなファイル) を中の fetch で取る
                import shlex
                q = shlex.quote(req['path'])
                r = vm.run('mkdir -p "$(dirname %s)" && fetch http://10.0.2.2:8001/%s -o %s >/dev/null' % (q, req['name'], q), 300)
            elif op == 'keys':
                vm.keys(req.get('keys', []))
                r = {'ok': True}
            elif op == 'type':
                vm.type(req['text'])
                r = {}
            elif op == 'mon':
                r = {'out': monitor(req['cmd'], req.get('wait', 0.5))}
            elif op == 'reboot':
                r = vm.reboot(req.get('timeout', 180), req.get('reset_after'), req.get('choose'))
            elif op == 'stop':
                vm.stop()
                r = {'stopped': True}
            else:
                r = {'error': 'unknown op %s' % op}
        except Exception as e:
            r = {'error': repr(e)}
        # 聞いた人が待ちきれずに切っていても、サーバーは止めない (止めると VM が取り残される)
        try:
            f.write(json.dumps(r) + '\n')
            f.flush()
        except OSError:
            pass
        c.close()
        if op == 'stop':
            os.unlink(CTL)
            return


def ask(req, timeout=None):
    c = socket.socket(socket.AF_UNIX)
    c.connect(CTL)
    c.settimeout(timeout)
    f = c.makefile('rw')
    f.write(json.dumps(req) + '\n')
    f.flush()
    return json.loads(f.readline())


def main():
    a = sys.argv[1:]
    op = a[0] if a else 'status'
    if op == 'serve':
        serve(a[1] if len(a) > 1 else None)
    elif op == 'start':
        if os.path.exists(CTL):
            try:
                print(json.dumps(ask({'op': 'status'}, 5)))
                return
            except OSError:
                # 前に動いていたものの残り (止めずに消えた)
                os.unlink(CTL)
        www = a[a.index('--www') + 1] if '--www' in a else ''
        os.makedirs(DIR, exist_ok=True)
        srv = subprocess.Popen([sys.executable, os.path.abspath(__file__), 'serve'] + ([os.path.abspath(www)] if www else []),
                               stdin=subprocess.DEVNULL, stdout=open(DIR + '/serve.log', 'w'), stderr=subprocess.STDOUT, start_new_session=True)
        # ctl ができる (= プロンプトまで来たか、あきらめた) まで待つ。serve が終わった (stop された、落ちた) らやめる
        end = time.time() + 960
        while not os.path.exists(CTL) and time.time() < end and srv.poll() is None:
            time.sleep(1)
        if not os.path.exists(CTL):
            print(json.dumps({'ready': False, 'error': 'the VM stopped before it was ready (stop, or see serve.log)'}))
            print(clean(open(DIR + '/serve.log', 'rb').read()[-2000:].decode('utf-8', 'replace')), file=sys.stderr)
            sys.exit(1)
        r = ask({'op': 'status'}, 10)
        print(json.dumps(r))
        if not r.get('ready'):
            print(r.get('out', ''), file=sys.stderr)
            sys.exit(1)
    elif op == 'run':
        timeout = float(a[a.index('-t') + 1]) if '-t' in a else 600
        r = ask({'op': 'run', 'cmd': a[1], 'timeout': timeout})
        if r.get('out'):
            print(r['out'])
        if 'error' in r or r.get('timeout'):
            print(json.dumps({k: v for k, v in r.items() if k != 'out'}), file=sys.stderr)
            sys.exit(124 if r.get('timeout') else 1)
        sys.exit(r['status'])
    elif op == 'bg':
        r = ask({'op': 'bg', 'cmd': a[1]})
        print(json.dumps(r))
        sys.exit(1 if 'error' in r else 0)
    elif op == 'job':
        # test/vm.py job [ID] [-w SEC] [--kill]
        req = {'op': 'job', 'wait': float(a[a.index('-w') + 1]) if '-w' in a else 0, 'kill': '--kill' in a}
        if len(a) > 1 and a[1].isdigit():
            req['id'] = int(a[1])
        r = ask(req)
        print(json.dumps(r))
        sys.exit(1 if 'error' in r else 0)
    elif op == 'type':
        ask({'op': 'type', 'text': a[1].replace('\\n', '\n')})
    elif op == 'keys':
        ask({'op': 'keys', 'keys': a[1:]})
    elif op == 'put':
        import base64
        data = open(a[1], 'rb').read()
        if len(data) > 256 << 20:
            sys.exit('vm: put is for files up to 256 MB')
        import gzip
        if len(data) > 16 << 10:
            # 大きいもの: シリアルは 1 秒に 15 KB ほど (197 KB で 14 秒) なので、http で (DIR/xfer に置いて中の fetch で取る。
            # 1 MB でも 1 秒ほど)
            name = 'put-%d-%s' % (os.getpid(), os.path.basename(a[1]))
            tmp = os.path.join(DIR, 'xfer', name)
            open(tmp, 'wb').write(data)
            try:
                r = ask({'op': 'fetch', 'path': a[2], 'name': name})
            finally:
                os.unlink(tmp)
        else:
            r = ask({'op': 'put', 'path': a[2], 'b64': base64.encodebytes(gzip.compress(data, 6)).decode(), 'gz': True})
        if r.get('status') != 0:
            sys.exit('vm: put: %s' % json.dumps(r))
    elif op == 'get':
        # 中の fetch は取ってくるだけで送れないので、シリアルで。base64 を何行にもすると、run の答えをそろえる tidy が
        # 同じ行のくり返し (0 の並びなど) をまとめてこわすので 1 行に (base64 -w 0)
        import base64, gzip, hashlib, shlex
        q = shlex.quote(a[1])
        r = ask({'op': 'run', 'cmd': 'sha256sum %s && stat -c %%s %s' % (q, q), 'timeout': 60})
        words = r.get('out', '').split()
        if r.get('status') != 0 or len(words) < 3 or not words[-1].isdigit():
            sys.exit('vm: get: %s: %s' % (a[1], r.get('out', '').strip() or json.dumps(r)))
        want, size = words[0], int(words[-1])
        if size > 16 << 20:
            sys.exit('vm: get is for files up to 16 MB (it comes over the serial line)')
        r = ask({'op': 'run', 'cmd': '{ gzip -c %s 2>/dev/null || toybox gzip -c %s; } | base64 -w 0; echo' % (q, q), 'timeout': 600})
        try:
            data = gzip.decompress(base64.b64decode(''.join(r.get('out', '').split())))
        except Exception as e:
            sys.exit('vm: get: %s: could not read what came (%s)' % (a[1], e))
        got = hashlib.sha256(data).hexdigest()
        if got != want:
            sys.exit('vm: get: %s: sha256 %s here but %s inside' % (a[1], got, want))
        out = os.path.abspath(a[2])
        os.makedirs(os.path.dirname(out), exist_ok=True)
        with open(out, 'wb') as f:
            f.write(data)
        print(json.dumps({'local': out, 'bytes': len(data), 'sha256': got}))
    elif op == 'mon':
        print(ask({'op': 'mon', 'cmd': a[1]})['out'])
    elif op == 'reboot':
        # test/vm.py reboot [-t SECS] [--reset-after SECS]
        req = {'op': 'reboot', 'timeout': float(a[a.index('-t') + 1]) if '-t' in a else 180}
        if '--reset-after' in a:
            req['reset_after'] = float(a[a.index('--reset-after') + 1])
        if '--choose' in a:
            req['choose'] = a[a.index('--choose') + 1]
        r = ask(req)
        print(json.dumps(r))
        sys.exit(0 if r.get('ready') else 1)
    elif op == 'shot':
        out = os.path.abspath(a[1] if len(a) > 1 else 'shot.png')
        ppm = out.rsplit('.', 1)[0] + '.ppm'
        ask({'op': 'mon', 'cmd': 'screendump ' + ppm, 'wait': 1.5})
        if out != ppm and subprocess.run(['convert', ppm, out], stderr=subprocess.DEVNULL).returncode == 0:
            os.unlink(ppm)
            print(out)
        else:
            print(ppm)
    elif op == 'log':
        n = int(a[1]) if len(a) > 1 else 40
        t = screen(open(LOG, 'rb').read()[-200000:].decode('utf-8', 'replace'))
        print('\n'.join(t.split('\n')[-n:]))
    elif op == 'status':
        if not os.path.exists(CTL):
            print('{"running":false}')
            return
        print(json.dumps(ask({'op': 'status'}, 10)))
    elif op == 'stop':
        if os.path.exists(CTL):
            try:
                print(json.dumps(ask({'op': 'stop'}, 60)))
                return
            except OSError:
                pass
        # サーバーが答えない (動かしたものが終わらないなど): サーバー (と http) と QEMU を、グループごと止める
        try:
            serve_pid, q_pid = map(int, open(DIR + '/pids').read().split())
        except (OSError, ValueError):
            return
        for kill in (lambda: os.killpg(serve_pid, signal.SIGKILL), lambda: os.killpg(q_pid, signal.SIGKILL)):
            try:
                kill()
            except ProcessLookupError:
                pass
        for f in (CTL, DIR + '/pids'):
            try:
                os.unlink(f)
            except FileNotFoundError:
                pass
        print('{"stopped": true, "killed": true}')
    else:
        sys.exit(__doc__ or 'usage: test/vm.py start|run|type|shot|mon|log|status|stop')


if __name__ == '__main__':
    try:
        main()
    except ConnectionRefusedError:
        sys.exit('vm: not started (test/vm.py start)')
    except FileNotFoundError as e:
        # ctl のソケットがない (connect には名前がつかない) のと、put で送るファイルがないのを分ける
        if e.filename and e.filename != CTL:
            sys.exit('vm: %s: no such file' % e.filename)
        sys.exit('vm: not started (test/vm.py start)')
