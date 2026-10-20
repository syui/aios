# ファイルまわりの速さ

aish の `bench` (kind fs、linux、record) が作る。もとは bench/fs.jsonl (1 行 1 回の記録)。測るのは test/fsbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-20T23:18:40Z の f9ae243 (r564)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| create | 236 µs | 235 µs | 1.00 | - |  | 空のファイルを O_CREAT|O_EXCL で作って閉じる |
| stat | 78.5 µs | 10.3 µs | 7.63 | - |  | 作ったばかりのファイルを名前で stat |
| stat-missing | 69.1 µs | 32.7 µs | 2.11 | - |  | ないファイルを stat (ENOENT) |
| stat-missing-again | 17.7 µs | 7.1 µs | 2.50 | - |  | 同じないものをもう一度 (PATH や #include を探すときのように) |
| open-close | 22.0 µs | 15.1 µs | 1.46 | - |  | あるファイルを開いて閉じる |
| write-4k | 137 µs | 98.6 µs | 1.39 | - |  | 小さなファイルに 4 KiB 書く |
| read-4k | 45.2 µs | 41.0 µs | 1.10 | - |  | 4 KiB 読む (ページキャッシュにある) |
| readdir | 1532 µs | 2222 µs | 0.69 | - |  | 1000 こ入ったディレクトリを読む |
| deep-stat | 13.7 µs | 7.8 µs | 1.75 | - |  | 10 段の深いパスの先を stat |
| rename | 283 µs | 255 µs | 1.11 | - |  | 名前を変える |
| unlink | 121 µs | 247 µs | 0.49 | - |  | 消す |
| mkdir-rmdir | 376 µs | 878 µs | 0.43 | - |  | ディレクトリを作って消す |
| write-seq | 172 MB/s | 58 MB/s | 0.34 | - |  | 16 MiB を 64 KiB ずつ書いて fsync |
| read-seq | 366 MB/s | 1784 MB/s | 4.88 | - |  | 16 MiB を 64 KiB ずつ読む (ページキャッシュにある) |
| write-fsync | 1157 µs | 5263 µs | 0.22 | - |  | 4 KiB 書いて fsync (ジャーナルの commit) を 50 回 |

## part と CPU を使っている区間 (aios、全部の op を 1 回、1 回あたり)

- part: extfs 60%, vfs 18%, sysfile 15%, block 3%, vm 3%, jbd2 1%
- 区間: `vfs:lookup` 15.1 µs × 14061、`extfs:create` 129.4 µs × 1514、`extfs:lookup` 12.2 µs × 15704、`extfs:write` 111.4 µs × 1306、`extfs:unlink` 93.9 µs × 1514、`extfs:rename` 96.3 µs × 1000、`sysfile:openat` 13.6 µs × 4025、`block:write` 93.7 µs × 484

## 直すところの候補

- stat: Linux の 7.63 倍の時間
- stat-missing: Linux の 2.11 倍の時間
- stat-missing-again: Linux の 2.50 倍の時間
- open-close: Linux の 1.46 倍の時間
- write-4k: Linux の 1.39 倍の時間
- read-4k: Linux の 1.10 倍の時間
- deep-stat: Linux の 1.75 倍の時間
- rename: Linux の 1.11 倍の時間
- read-seq: Linux の 4.88 倍の時間
