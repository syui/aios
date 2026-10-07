#!/usr/bin/env python3
# aish --mcp のテスト (test/aish.sh から): python3 test/aish-mcp.py AISH PLUGINS
#   run、長い出力を切って out で読む、job の新しい分だけ、edit と sed の答え、where の body、ツールのスキーマ (check)、
#   ビルドしなおしたときの入れかわり (相対パスの RC を読んでから cd しても、プラグインがそろう)
import json, os, shutil, subprocess, sys, tempfile, time

aish, plugins = sys.argv[1], sys.argv[2]
work = tempfile.mkdtemp()
fails = []


def check(name, ok, detail=''):
    if not ok:
        fails.append(name)
        print('FAIL', name, detail[:400])


# 入れかわりをためすので、aish とプラグインはコピーを使う
os.makedirs(work + '/plug')
shutil.copy(aish, work + '/aish')
for p in ['aish-edit', 'aish-map', 'aish-wait']:
    shutil.copy(os.path.join(plugins, p), work + '/plug/' + p)
open(work + '/my.rc', 'w').write('plugin aish-edit\nplugin aish-map\nplugin aish-wait\nsetopt ksharrays\n')
os.makedirs(work + '/home')
env = {'PATH': '/usr/bin:/bin', 'HOME': work + '/home', 'AISH_PLUGIN_PATH': work + '/plug'}
p = subprocess.Popen([work + '/aish', '--mcp', 'my.rc'], cwd=work, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)
notes = []


def send(o):
    p.stdin.write(json.dumps(o) + '\n')
    p.stdin.flush()


def recv():
    while True:
        m = json.loads(p.stdout.readline())
        if 'id' in m:
            return m
        notes.append(m.get('method'))


ids = iter(range(1, 10000))
send({'jsonrpc': '2.0', 'id': next(ids), 'method': 'initialize', 'params': {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 't', 'version': '0'}}})
init = recv()['result']
check('initialize listChanged', init['capabilities']['tools'].get('listChanged') is True, str(init))


def tool(name, args):
    send({'jsonrpc': '2.0', 'id': next(ids), 'method': 'tools/call', 'params': {'name': name, 'arguments': args}})
    return recv()['result']['content'][0]['text']


send({'jsonrpc': '2.0', 'id': next(ids), 'method': 'tools/list'})
tools = {t['name']: t for t in recv()['result']['tools']}
for want in ['run', 'job', 'out', 'check', 'read', 'edit', 'sed', 'grep', 'where', 'wait']:
    check('tool ' + want, want in tools)
check('sed schema has properties', 'pattern' in tools.get('sed', {}).get('inputSchema', {}).get('properties', {}), json.dumps(tools.get('sed')))

t = tool('check', {})
check('check ok', '"ok":true' in t, t)

t = tool('run', {'cmd': 'echo hi; a=(x y z); echo ${a[0]}'})
check('run', t.startswith('hi\nx\n'), t)

t = tool('run', {'cmd': 'seq 1 30000'})
check('long output is cut', 'lines,' in t and 'out_id' in t and len(t) < 14000, t[:300])
oid = json.loads(t.strip().splitlines()[-1])['out_id']
t = tool('out', {'id': oid, 'grep': '15000'})
check('out grep', t.startswith('15000: 15000\n'), t)
t = tool('out', {'id': oid, 'from': 20000, 'to': 20001})
check('out range', t.startswith('20000: 20000\n20001: 20001\n'), t)

t = tool('run', {'cmd': 'for i in 1 2 3; do echo s$i; sleep 0.5; done', 'bg': True})
job = json.loads(t)['job']
t1 = tool('job', {'id': job, 'wait_ms': 700})
t2 = tool('job', {'id': job, 'wait_ms': 5000})
check('job only new output', 's1' in t1 and 's1' not in t2 and 's3' in t2 and 'shown_before' in t2, t1 + ' / ' + t2)

f = work + '/a.txt'
open(f, 'w').write('one\ntwo\nthree\nfour\n')
t = tool('edit', {'path': f, 'old': 'two', 'new': 'TWO'})
check('edit shows lines', '2\tTWO' in t and '"lines":[2]' in t, t)
t = tool('sed', {'path': f, 'pattern': 'four', 'replace': 'FOUR', 'count': 1})
check('sed shows lines', '4\tFOUR' in t, t)

# word: 単語ぴったり (rg でも aish の中のものでも)
open(work + '/w.txt', 'w').write('a tool\ntools\ntool_once\n')
t = tool('grep', {'pattern': 'tool', 'path': work + '/w.txt', 'word': True})
check('grep word', '"count":1' in t and 'tools' not in t, t)

# where の body: 定義の中身 ('{' の文字や文字列の中のかっこはかぞえない)
open(work + '/m.rs', 'w').write('fn other() {}\n\nfn target(x: char) -> bool {\n    let s = "}";\n    x == \'{\'\n}\nfn after() {}\n')
t = tool('where', {'name': 'target', 'path': work, 'body': True})
check('where body', 'needs rg' in t or '5\t    x == ' in t and '6\t}' in t and 'after' not in t, t)

# tool: プラグインのツールをシェルから (パイプの中、$(...) からも)
t = tool('run', {'cmd': 'tool read path=a.txt limit=1; tool read \'{"path":"a.txt","offset":2,"limit":1}\' | cat; x=$(tool read path=a.txt limit=1); echo "[$x]" | head -1; tool nosuch; echo st=$?'})
check('tool builtin', t.startswith('     1\tone\n     2\tTWO\n[     1\tone]\nst=1\n'), t)

# ビルドしなおし (新しい i-node) → 次のツールで入れかわる。cd したあとでも RC (相対パス) は読める
tool('run', {'cmd': 'cd /tmp'})
shutil.copy(work + '/plug/aish-edit', work + '/plug/n')
os.rename(work + '/plug/n', work + '/plug/aish-edit')
time.sleep(2.5)
t = tool('read', {'path': f, 'limit': 1})
check('reload', 'reloaded' in t and 'only' not in t, t)
check('reload notifies', 'notifications/tools/list_changed' in notes, str(notes))
t = tool('check', {})
check('reload keeps plugins', 'plugins 3/3 alive' in t, t)
t = tool('run', {'cmd': 'pwd; a=(x y); echo ${a[0]}'})
check('reload keeps dir and rc', t.startswith('/tmp\nx\n'), t)

p.stdin.close()
p.wait(timeout=10)
shutil.rmtree(work, ignore_errors=True)
print('mcp: %d failed%s' % (len(fails), (' (' + ', '.join(fails) + ')') if fails else ''))
sys.exit(1 if fails else 0)
