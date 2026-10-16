#!/usr/bin/env python3
# kernel/src/crypto.rs (乱数の部品の ChaCha20 と BLAKE2s) の答え合わせ。CI と手元で: python3 test/crypto.py
# 開発のマシンの rustc でビルドして (crypto.rs は core のほかを使わない)、
#   BLAKE2s: RFC 7693 の例 ("abc") と、Python の hashlib.blake2s (鍵なし、鍵つき、いろいろな長さ、update を分けて)
#   ChaCha20: RFC 8439 の例 (2.3.2 のブロック) と、openssl enc -chacha20 の鍵の流れ (あれば)
# とくらべる。終わりの番号はくいちがいの数
import hashlib
import os
import random
import shutil
import subprocess
import sys
import tempfile

src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kernel", "src", "crypto.rs")
HARNESS = r'''
#[path = "%s"]
#[allow(dead_code)]
mod crypto;
use std::io::BufRead;
fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{:02x}", x)).collect() }
fn unhex(s: &str) -> Vec<u8> { if s == "-" { return vec![]; } (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect() }
fn main() {
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let w: Vec<&str> = line.split(' ').collect();
        match w[0] {
            // b KEY MSG STEP: BLAKE2s (update は STEP バイトずつ)
            "b" => {
                let (k, m, step) = (unhex(w[1]), unhex(w[2]), w[3].parse::<usize>().unwrap());
                let mut h = crypto::Blake2s::new(&k);
                for c in m.chunks(step) { h.update(c); }
                println!("{}", hex(&h.finish()));
            }
            // c KEY COUNTER NONCE BLOCKS: ChaCha20 の鍵の流れ
            "c" => {
                let k: [u8; 32] = unhex(w[1]).try_into().unwrap();
                let n: [u8; 12] = unhex(w[3]).try_into().unwrap();
                let mut s = crypto::chacha_state(&k, w[2].parse().unwrap(), &n);
                let mut b = [0u8; 64];
                let mut out = String::new();
                for _ in 0..w[4].parse::<usize>().unwrap() { crypto::chacha20_block(&mut s, &mut b); out += &hex(&b); }
                println!("{}", out);
            }
            _ => println!("?"),
        }
    }
}
'''

errors = 0


def check(what, got, want):
    global errors
    if got != want:
        errors += 1
        print("FAIL %s:\n  got  %s\n  want %s" % (what, got, want))


tmp = tempfile.mkdtemp(prefix="crypto-")
try:
    with open(os.path.join(tmp, "kat.rs"), "w") as f:
        f.write(HARNESS % os.path.abspath(src))
    exe = os.path.join(tmp, "kat")
    subprocess.run(["rustc", "--edition", "2024", "-O", "-o", exe, os.path.join(tmp, "kat.rs")], check=True)
    rnd = random.Random(20261016)
    cases = []  # (行, 答え, 名前)

    # BLAKE2s
    cases.append(("b - 616263 64", "508c5e8c327c14e2e1a72ba34eeb452f37458b209ed63a294d999b4c86675982", "blake2s rfc7693 abc"))
    for n in (0, 1, 3, 31, 32, 55, 63, 64, 65, 127, 128, 129, 200, 1000):
        for key in (b"", bytes(range(32)), bytes(rnd.randrange(256) for _ in range(rnd.choice((1, 16, 31))))):
            msg = bytes(rnd.randrange(256) for _ in range(n))
            step = rnd.choice((1, 7, 64, 1000))
            want = hashlib.blake2s(msg, key=key, digest_size=32).hexdigest()
            cases.append(("b %s %s %d" % (key.hex() or "-", msg.hex() or "-", step), want, "blake2s len %d key %d step %d" % (n, len(key), step)))

    # ChaCha20: RFC 8439 2.3.2
    rfc = ("10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e"
           "d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e")
    cases.append(("c %s 1 000000090000004a00000000 1" % bytes(range(32)).hex(), rfc, "chacha20 rfc8439 2.3.2"))
    ssl = shutil.which("openssl")
    streams = []
    for i in range(8):
        key = bytes(rnd.randrange(256) for _ in range(32))
        nonce = bytes(12) if i < 2 else bytes(rnd.randrange(256) for _ in range(12))
        ctr = (0, 1, 7, 1000)[i % 4]
        streams.append((key, ctr, nonce))
    if ssl:
        for key, ctr, nonce in streams:
            iv = ctr.to_bytes(4, "little") + nonce
            r = subprocess.run([ssl, "enc", "-chacha20", "-K", key.hex(), "-iv", iv.hex(), "-nosalt"], input=bytes(64 * 3), capture_output=True)
            if r.returncode != 0:
                print("openssl: %s (ChaCha20 is checked by the RFC example only)" % r.stderr.decode().strip())
                break
            cases.append(("c %s %d %s 3" % (key.hex(), ctr, nonce.hex()), r.stdout.hex(), "chacha20 vs openssl counter %d" % ctr))
    else:
        print("openssl: not found (ChaCha20 is checked by the RFC example only)")

    r = subprocess.run([exe], input="".join(c[0] + "\n" for c in cases), capture_output=True, text=True, check=True)
    got = r.stdout.split("\n")
    for i, (_, want, what) in enumerate(cases):
        check(what, got[i] if i < len(got) else "", want)
    print("crypto: %d checks, %d failed" % (len(cases), errors))
finally:
    shutil.rmtree(tmp, ignore_errors=True)
sys.exit(min(errors, 125))
