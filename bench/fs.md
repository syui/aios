# ファイルまわりの速さ

aish の `bench` (kind fs、linux、record) が作る。もとは bench/fs.jsonl (1 行 1 回の記録)。測るのは test/fsbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-21T03:19:28Z の 52c73f2 (r578)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| create | 214 µs | 235 µs | 0.91 | 1.11 |  | 空のファイルを O_CREAT|O_EXCL で作って閉じる |
| stat | 15.0 µs | 11.5 µs | 1.30 | 0.26 |  | 作ったばかりのファイルを名前で stat |
| stat-missing | 51.7 µs | 34.0 µs | 1.52 | 0.96 |  | ないファイルを stat (ENOENT) |
| stat-missing-again | 20.7 µs | 7.6 µs | 2.74 | 1.57 |  | 同じないものをもう一度 (PATH や #include を探すときのように) |
| open-close | 19.0 µs | 18.6 µs | 1.02 | 1.07 |  | あるファイルを開いて閉じる |
| write-4k | 115 µs | 106 µs | 1.09 | 1.00 |  | 小さなファイルに 4 KiB 書く |
| read-4k | 37.6 µs | 47.8 µs | 0.79 | 1.00 |  | 4 KiB 読む (ページキャッシュにある) |
| readdir | 1252 µs | 2614 µs | 0.48 | 1.07 |  | 1000 こ入ったディレクトリを読む |
| deep-stat | 14.1 µs | 8.4 µs | 1.69 | 1.11 |  | 10 段の深いパスの先を stat |
| rename | 286 µs | 271 µs | 1.06 | 1.05 |  | 名前を変える |
| unlink | 100 µs | 303 µs | 0.33 | 1.00 |  | 消す |
| mkdir-rmdir | 380 µs | 995 µs | 0.38 | 1.11 |  | ディレクトリを作って消す |
| write-seq | 196 MB/s | 51 MB/s | 0.26 | 0.98 |  | 16 MiB を 64 KiB ずつ書いて fsync |
| read-seq | 472 MB/s | 1721 MB/s | 3.64 | 1.08 |  | 16 MiB を 64 KiB ずつ読む (ページキャッシュにある) |
| write-fsync | 1231 µs | 5258 µs | 0.23 | 1.19 |  | 4 KiB 書いて fsync (ジャーナルの commit) を 50 回 |

## part と CPU を使っている区間 (aios、全部の op を 1 回、1 回あたり)

- part: extfs 60%, sysfile 19%, vfs 10%, block 6%, vm 4%, jbd2 1%
- 区間: `extfs:create` 125.2 µs × 1514、`extfs:lookup` 18.8 µs × 9430、`extfs:write` 107.0 µs × 1306、`vfs:lookup` 9.4 µs × 14061、`extfs:rename` 106.8 µs × 1000、`extfs:unlink` 59.9 µs × 1514、`block:write` 168.2 µs × 484、`sysfile:renameat` 70.5 µs × 1000

## 直すところの候補

- create: 前の記録 (6b0a13c) の 1.11 倍の時間
- stat: Linux の 1.30 倍の時間
- stat-missing: Linux の 1.52 倍の時間
- stat-missing-again: Linux の 2.74 倍の時間
- stat-missing-again: 前の記録 (6b0a13c) の 1.57 倍の時間
- deep-stat: Linux の 1.69 倍の時間
- deep-stat: 前の記録 (6b0a13c) の 1.11 倍の時間
- mkdir-rmdir: 前の記録 (6b0a13c) の 1.11 倍の時間
- read-seq: Linux の 3.64 倍の時間
- write-fsync: 前の記録 (6b0a13c) の 1.19 倍の時間

## これまで (同じ VM の記録。aios の値、かっこは aios / Linux)

| 記録 | create | stat | stat-missing | stat-missing-again | open-close | write-4k | read-4k | readdir | deep-stat | rename | unlink | mkdir-rmdir | write-seq | read-seq | write-fsync |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-10-21 52c73f2 | 214 µs (0.91) | 15.0 µs (1.30) | 51.7 µs (1.52) | 20.7 µs (2.74) | 19.0 µs (1.02) | 115 µs (1.09) | 37.6 µs (0.79) | 1252 µs (0.48) | 14.1 µs (1.69) | 286 µs (1.06) | 100 µs (0.33) | 380 µs (0.38) | 196 MB/s (0.26) | 472 MB/s (3.64) | 1231 µs (0.23) |
| 2026-10-21 6b0a13c | 192 µs (0.83) | 58.0 µs (5.62) | 53.7 µs (1.35) | 13.2 µs (1.58) | 17.7 µs (1.10) | 115 µs (0.84) | 37.6 µs (0.93) | 1171 µs (0.50) | 12.7 µs (1.60) | 272 µs (1.03) | 100 µs (0.37) | 342 µs (0.35) | 193 MB/s (0.28) | 512 MB/s (2.56) | 1033 µs (0.19) |
| 2026-10-20 f9ae243 | 236 µs (1.00) | 78.5 µs (7.63) | 69.1 µs (2.11) | 17.7 µs (2.50) | 22.0 µs (1.46) | 137 µs (1.39) | 45.2 µs (1.10) | 1532 µs (0.69) | 13.7 µs (1.75) | 283 µs (1.11) | 121 µs (0.49) | 376 µs (0.43) | 172 MB/s (0.34) | 366 MB/s (4.88) | 1157 µs (0.22) |
