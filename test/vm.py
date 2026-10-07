#!/usr/bin/env python3
# aios を QEMU で起こしたままにして、外からコマンドを送る (人の手の代わり。Claude が aios の中で確かめるため)
#
#   test/vm.py start [--www DIR]   起こして、プロンプトが出るまで待つ (bin/run.sh。画面は AIOS_DISPLAY=none。
#                                  AIOS_KERNEL=FILE でそのカーネルを。ほかの AIOS_* も run.sh へ)
#   test/vm.py run 'CMD' [-t SEC]  シリアルの端末で動かし、出力を出して、同じ終わりの番号で終わる
#   test/vm.py type 'TEXT'         画面のキーボードで打つ (\n で Enter。aiwm の上の aiterm など)
#   test/vm.py shot FILE.png       画面を撮る (png は ImageMagick の convert があれば。なければ .ppm)
#   test/vm.py mon 'CMD'           QEMU のモニタへ
#   test/vm.py log [N]             シリアルの終わりの N 行 (既定 40)
#   test/vm.py status / stop
#
# 起こしたものは、うしろの小さなサーバー (serve) が持つ。シリアルはいつも読みつづけるので、カーネルの
# 出力でつまらない。--www DIR で DIR を http://10.0.2.2:8000/ に出す (aios の中から fetch で取れる)。
# 置き場は $AIOS_VM_DIR (既定 /tmp/aios-vm-UID): ctl (サーバーの口)、mon (QEMU のモニタ)、serial.log
import json, os, pty, re, select, signal, socket, subprocess, sys, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.environ.get('AIOS_VM_DIR') or '/tmp/aios-vm-%d' % os.getuid()
CTL, MON, LOG = DIR + '/ctl', DIR + '/mon', DIR + '/serial.log'
PROMPT = re.compile(rb'aios[^\n]*\][$#] ')
ANSI = re.compile(r'\x1b\[[0-9;?]*[A-Za-z]|\x1b[()][0-9A-Za-z]|\r')
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
    def __init__(self, www):
        self.buf = b''
        self.cv = threading.Condition()
        self.log = open(LOG, 'wb')
        self.http = subprocess.Popen([sys.executable, '-m', 'http.server', '8000', '-d', www], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) if www else None
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
                self.cv.notify_all()

    dead = False

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
        os.write(self.m, (cmd + '; echo ' + tag + ' $?\r').encode())
        out, mm = self.wait_for(re.compile((tag + r' (\d+)\r*\n').encode()), timeout)
        if out is None:
            # 止める (Ctrl-C) と、プロンプトに戻るのを少し待つ
            os.write(self.m, b'\x03')
            self.wait_for(PROMPT, 5)
            t = clean(self.buf.decode('utf-8', 'replace'))
            i = t.rfind(tag + ' $?')
            return {'timeout': True, 'out': (t[t.find('\n', i) + 1:] if i >= 0 and '\n' in t[i:] else t)[-4000:]}
        # 色 (aish-highlight) を消してから、打った行のこだま (... echo TAG $?) のあとを。
        # aish は 1 文字ごとに行を描きなおすので、いちばん終わりのもの
        text = clean(out[:mm.start()].decode('utf-8', 'replace'))
        i = text.rfind(tag + ' $?')
        if i >= 0:
            text = text[text.find('\n', i) + 1:] if '\n' in text[i:] else ''
        return {'status': int(mm.group(1)), 'out': clean(text)}

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


def clean(t):
    return ANSI.sub('', t).strip('\n')


def serve(www):
    os.makedirs(DIR, exist_ok=True)
    for f in (CTL, MON):
        try:
            os.unlink(f)
        except FileNotFoundError:
            pass
    vm = Serve(www)
    t0 = time.time()
    ok, _ = vm.wait_for(PROMPT, 900)
    ready = {'ready': ok is not None, 'boot_s': round(time.time() - t0, 1)}
    if ok is None:
        ready['out'] = clean(open(LOG, 'rb').read()[-3000:].decode('utf-8', 'replace'))
    time.sleep(2)
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
                r = dict(ready, error='aios did not reach the prompt (see log)')
            elif op == 'run':
                r = vm.run(req['cmd'], req.get('timeout', 600))
            elif op == 'type':
                vm.type(req['text'])
                r = {}
            elif op == 'mon':
                r = {'out': monitor(req['cmd'], req.get('wait', 0.5))}
            elif op == 'stop':
                vm.stop()
                r = {'stopped': True}
            else:
                r = {'error': 'unknown op %s' % op}
        except Exception as e:
            r = {'error': repr(e)}
        f.write(json.dumps(r) + '\n')
        f.flush()
        c.close()
        if op == 'stop':
            os.unlink(CTL)
            return


def ask(req, timeout=None):
    c = socket.socket(socket.AF_UNIX)
    try:
        c.connect(CTL)
    except OSError:
        sys.exit('vm: not started (test/vm.py start)')
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
                os.unlink(CTL)
        www = a[a.index('--www') + 1] if '--www' in a else ''
        os.makedirs(DIR, exist_ok=True)
        subprocess.Popen([sys.executable, os.path.abspath(__file__), 'serve'] + ([os.path.abspath(www)] if www else []),
                         stdin=subprocess.DEVNULL, stdout=open(DIR + '/serve.log', 'w'), stderr=subprocess.STDOUT, start_new_session=True)
        # ctl ができる (= プロンプトまで来たか、あきらめた) まで待つ
        end = time.time() + 960
        while not os.path.exists(CTL) and time.time() < end:
            time.sleep(1)
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
    elif op == 'type':
        ask({'op': 'type', 'text': a[1].replace('\\n', '\n')})
    elif op == 'mon':
        print(ask({'op': 'mon', 'cmd': a[1]})['out'])
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
        t = clean(open(LOG, 'rb').read()[-200000:].decode('utf-8', 'replace'))
        print('\n'.join(t.split('\n')[-n:]))
    elif op == 'status':
        if not os.path.exists(CTL):
            print('{"running":false}')
            return
        print(json.dumps(ask({'op': 'status'}, 10)))
    elif op == 'stop':
        if os.path.exists(CTL):
            print(json.dumps(ask({'op': 'stop'}, 120)))
    else:
        sys.exit(__doc__ or 'usage: test/vm.py start|run|type|shot|mon|log|status|stop')


if __name__ == '__main__':
    main()
