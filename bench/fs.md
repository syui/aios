# ファイルまわりの速さ

aish の `bench` (kind fs、linux、record) が作る。もとは bench/fs.jsonl (1 行 1 回の記録)。測るのは test/fsbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-21T02:20:03Z の 6b0a13c (r574)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| create | 192 µs | 233 µs | 0.83 | 0.81 |  | 空のファイルを O_CREAT|O_EXCL で作って閉じる |
| stat | 58.0 µs | 10.3 µs | 5.62 | 0.74 |  | 作ったばかりのファイルを名前で stat |
| stat-missing | 53.7 µs | 39.9 µs | 1.35 | 0.78 |  | ないファイルを stat (ENOENT) |
| stat-missing-again | 13.2 µs | 8.3 µs | 1.58 | 0.74 |  | 同じないものをもう一度 (PATH や #include を探すときのように) |
| open-close | 17.7 µs | 16.1 µs | 1.10 | 0.80 |  | あるファイルを開いて閉じる |
| write-4k | 115 µs | 136 µs | 0.84 | 0.84 |  | 小さなファイルに 4 KiB 書く |
| read-4k | 37.6 µs | 40.3 µs | 0.93 | 0.83 |  | 4 KiB 読む (ページキャッシュにある) |
| readdir | 1171 µs | 2361 µs | 0.50 | 0.76 |  | 1000 こ入ったディレクトリを読む |
| deep-stat | 12.7 µs | 7.9 µs | 1.60 | 0.93 |  | 10 段の深いパスの先を stat |
| rename | 272 µs | 264 µs | 1.03 | 0.96 |  | 名前を変える |
| unlink | 100 µs | 271 µs | 0.37 | 0.83 |  | 消す |
| mkdir-rmdir | 342 µs | 969 µs | 0.35 | 0.91 |  | ディレクトリを作って消す |
| write-seq | 193 MB/s | 55 MB/s | 0.28 | 0.89 |  | 16 MiB を 64 KiB ずつ書いて fsync |
| read-seq | 512 MB/s | 1313 MB/s | 2.56 | 0.71 |  | 16 MiB を 64 KiB ずつ読む (ページキャッシュにある) |
| write-fsync | 1033 µs | 5560 µs | 0.19 | 0.89 |  | 4 KiB 書いて fsync (ジャーナルの commit) を 50 回 |

## part と CPU を使っている区間 (aios、全部の op を 1 回、1 回あたり)

- part: extfs 60%, vfs 17%, sysfile 15%, block 4%, vm 3%, jbd2 1%
- 区間: `vfs:lookup` 13.3 µs × 14061、`extfs:lookup` 11.5 µs × 15704、`extfs:create` 113.1 µs × 1514、`extfs:write` 103.2 µs × 1306、`extfs:rename` 101.8 µs × 1000、`extfs:unlink` 64.2 µs × 1514、`sysfile:openat` 12.6 µs × 4025、`block:write` 96.3 µs × 484

## 直すところの候補

- stat: Linux の 5.62 倍の時間
- stat-missing: Linux の 1.35 倍の時間
- stat-missing-again: Linux の 1.58 倍の時間
- deep-stat: Linux の 1.60 倍の時間
- read-seq: Linux の 2.56 倍の時間

## これまで (同じ VM の記録。aios の値、かっこは aios / Linux)

| 記録 | create | stat | stat-missing | stat-missing-again | open-close | write-4k | read-4k | readdir | deep-stat | rename | unlink | mkdir-rmdir | write-seq | read-seq | write-fsync |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-10-21 6b0a13c | 192 µs (0.83) | 58.0 µs (5.62) | 53.7 µs (1.35) | 13.2 µs (1.58) | 17.7 µs (1.10) | 115 µs (0.84) | 37.6 µs (0.93) | 1171 µs (0.50) | 12.7 µs (1.60) | 272 µs (1.03) | 100 µs (0.37) | 342 µs (0.35) | 193 MB/s (0.28) | 512 MB/s (2.56) | 1033 µs (0.19) |
| 2026-10-20 f9ae243 | 236 µs (1.00) | 78.5 µs (7.63) | 69.1 µs (2.11) | 17.7 µs (2.50) | 22.0 µs (1.46) | 137 µs (1.39) | 45.2 µs (1.10) | 1532 µs (0.69) | 13.7 µs (1.75) | 283 µs (1.11) | 121 µs (0.49) | 376 µs (0.43) | 172 MB/s (0.34) | 366 MB/s (4.88) | 1157 µs (0.22) |
