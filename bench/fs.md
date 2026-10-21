# ファイルまわりの速さ

aish の `bench` (kind fs、linux、record) が作る。もとは bench/fs.jsonl (1 行 1 回の記録)。測るのは test/fsbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-21T10:59:12Z の 7ac5585 (r581)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| create | 251 µs | 260 µs | 0.97 | 1.18 |  | 空のファイルを O_CREAT|O_EXCL で作って閉じる |
| stat | 19.8 µs | 12.2 µs | 1.63 | 1.32 |  | 作ったばかりのファイルを名前で stat |
| stat-missing | 60.9 µs | 40.2 µs | 1.51 | 1.18 |  | ないファイルを stat (ENOENT) |
| stat-missing-again | 14.9 µs | 8.4 µs | 1.77 | 0.72 |  | 同じないものをもう一度 (PATH や #include を探すときのように) |
| open-close | 23.0 µs | 18.8 µs | 1.23 | 1.21 |  | あるファイルを開いて閉じる |
| write-4k | 151 µs | 115 µs | 1.31 | 1.31 |  | 小さなファイルに 4 KiB 書く |
| read-4k | 30.0 µs | 50.7 µs | 0.59 | 0.80 |  | 4 KiB 読む (ページキャッシュにある) |
| readdir | 1763 µs | 2783 µs | 0.63 | 1.41 |  | 1000 こ入ったディレクトリを読む |
| deep-stat | 17.7 µs | 9.7 µs | 1.82 | 1.26 |  | 10 段の深いパスの先を stat |
| rename | 418 µs | 293 µs | 1.43 | 1.46 |  | 名前を変える |
| unlink | 127 µs | 275 µs | 0.46 | 1.27 |  | 消す |
| mkdir-rmdir | 364 µs | 906 µs | 0.40 | 0.96 |  | ディレクトリを作って消す |
| write-seq | 171 MB/s | 52 MB/s | 0.31 | 1.15 |  | 16 MiB を 64 KiB ずつ書いて fsync |
| read-seq | 2051 MB/s | 1544 MB/s | 0.75 | 0.23 |  | 16 MiB を 64 KiB ずつ読む (ページキャッシュにある) |
| write-fsync | 1090 µs | 4536 µs | 0.24 | 0.89 |  | 4 KiB 書いて fsync (ジャーナルの commit) を 50 回 |

## part と CPU を使っている区間 (aios、全部の op を 1 回、1 回あたり)

- part: extfs 66%, sysfile 18%, vfs 11%, block 4%, jbd2 1%, proc 0%
- 区間: `extfs:create` 142.0 µs × 1514、`extfs:lookup` 19.4 µs × 9384、`extfs:write` 119.0 µs × 1306、`vfs:lookup` 9.7 µs × 14061、`extfs:rename` 105.9 µs × 1000、`extfs:unlink` 65.2 µs × 1514、`sysfile:openat` 19.8 µs × 4025、`sysfile:renameat` 56.5 µs × 1000

## 直すところの候補

- create: 前の記録 (52c73f2) の 1.18 倍の時間
- stat: Linux の 1.63 倍の時間
- stat: 前の記録 (52c73f2) の 1.32 倍の時間
- stat-missing: Linux の 1.51 倍の時間
- stat-missing: 前の記録 (52c73f2) の 1.18 倍の時間
- stat-missing-again: Linux の 1.77 倍の時間
- open-close: Linux の 1.23 倍の時間
- open-close: 前の記録 (52c73f2) の 1.21 倍の時間
- write-4k: Linux の 1.31 倍の時間
- write-4k: 前の記録 (52c73f2) の 1.31 倍の時間
- readdir: 前の記録 (52c73f2) の 1.41 倍の時間
- deep-stat: Linux の 1.82 倍の時間
- deep-stat: 前の記録 (52c73f2) の 1.26 倍の時間
- rename: Linux の 1.43 倍の時間
- rename: 前の記録 (52c73f2) の 1.46 倍の時間
- unlink: 前の記録 (52c73f2) の 1.27 倍の時間
- write-seq: 前の記録 (52c73f2) の 1.15 倍の時間

## これまで (同じ VM の記録。aios の値、かっこは aios / Linux)

| 記録 | create | stat | stat-missing | stat-missing-again | open-close | write-4k | read-4k | readdir | deep-stat | rename | unlink | mkdir-rmdir | write-seq | read-seq | write-fsync |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-10-21 7ac5585 | 251 µs (0.97) | 19.8 µs (1.63) | 60.9 µs (1.51) | 14.9 µs (1.77) | 23.0 µs (1.23) | 151 µs (1.31) | 30.0 µs (0.59) | 1763 µs (0.63) | 17.7 µs (1.82) | 418 µs (1.43) | 127 µs (0.46) | 364 µs (0.40) | 171 MB/s (0.31) | 2051 MB/s (0.75) | 1090 µs (0.24) |
| 2026-10-21 52c73f2 | 214 µs (0.91) | 15.0 µs (1.30) | 51.7 µs (1.52) | 20.7 µs (2.74) | 19.0 µs (1.02) | 115 µs (1.09) | 37.6 µs (0.79) | 1252 µs (0.48) | 14.1 µs (1.69) | 286 µs (1.06) | 100 µs (0.33) | 380 µs (0.38) | 196 MB/s (0.26) | 472 MB/s (3.64) | 1231 µs (0.23) |
| 2026-10-21 6b0a13c | 192 µs (0.83) | 58.0 µs (5.62) | 53.7 µs (1.35) | 13.2 µs (1.58) | 17.7 µs (1.10) | 115 µs (0.84) | 37.6 µs (0.93) | 1171 µs (0.50) | 12.7 µs (1.60) | 272 µs (1.03) | 100 µs (0.37) | 342 µs (0.35) | 193 MB/s (0.28) | 512 MB/s (2.56) | 1033 µs (0.19) |
| 2026-10-20 f9ae243 | 236 µs (1.00) | 78.5 µs (7.63) | 69.1 µs (2.11) | 17.7 µs (2.50) | 22.0 µs (1.46) | 137 µs (1.39) | 45.2 µs (1.10) | 1532 µs (0.69) | 13.7 µs (1.75) | 283 µs (1.11) | 121 µs (0.49) | 376 µs (0.43) | 172 MB/s (0.34) | 366 MB/s (4.88) | 1157 µs (0.22) |
