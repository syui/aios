# ループバックの速さ (TCP、UDP、AF_UNIX、パイプ)

aish の `bench` (kind net、linux、record) が作る。もとは bench/net.jsonl (1 行 1 回の記録)。測るのは test/netbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-21T01:48:45Z の 1de41f7 (r570)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| pipe_rr | 101 µs | 119 µs | 0.84 | 0.93 | sysfile 40%, file 32%, proc 28% | パイプで 1 バイトの往復 (2 つのプロセス。くらべるための土台: プロセスの切りかえと起こし) |
| futex_rr | 15.9 µs | 31.1 µs | 0.51 | - | futex 98%, sysfile 1%, vfs 1% | 2 つのスレッドが futex で交互に起こしあう往復 (スレッドの切りかえと futex の起こしだけ) |
| unix_rr | 106 µs | 234 µs | 0.46 | 0.90 | unix 64%, sysfile 28%, proc 8% | AF_UNIX の stream で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr | 89.0 µs | 502 µs | 0.18 | 0.86 | inet 62%, sysfile 31%, proc 7% | TCP (127.0.0.1、TCP_NODELAY) で 1 バイトの往復 (2 つのプロセス) |
| udp_rr | 91.6 µs | 366 µs | 0.25 | 0.89 | inet 61%, sysfile 31%, proc 8% | UDP (つないだもの) で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr_epoll | 128 µs | 488 µs | 0.26 | 0.90 | epoll 54%, inet 27%, sysfile 14% | TCP の往復で、読む前に epoll_wait で待つ (サーバーのよくある形) |
| unix_local | 28.8 µs | 32.9 µs | 0.88 | 1.16 | unix 60%, sysfile 33%, proc 7% | AF_UNIX の往復を 1 つのプロセスで (切りかえなし: ソケットの道だけ) |
| tcp_local | 23.6 µs | 114 µs | 0.21 | 1.12 | inet 57%, sysfile 34%, proc 9% | TCP の往復を 1 つのプロセスで |
| udp_local | 26.5 µs | 81.1 µs | 0.33 | 1.30 | inet 57%, sysfile 33%, proc 9% | UDP の往復を 1 つのプロセスで |
| unix_stream | 277 MB/s | 432 MB/s | 1.56 | 1.00 | unix 68%, sysfile 27%, proc 4% | AF_UNIX に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream | 358 MB/s | 303 MB/s | 0.85 | 0.98 | inet 73%, sysfile 25%, proc 1% | TCP に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream_4k | 240 MB/s | 48 MB/s | 0.20 | 0.96 | inet 69%, sysfile 26%, proc 4% | TCP に 4 KiB ずつ書いて、別のプロセスが読む |
| tcp_connect | 38.9 µs | 996 µs | 0.04 | 0.89 | inet 48%, socket 32%, proc 11% | connect、accept、両方を閉じる (1 つのつながり) |

## CPU を使っている区間 (aios、op ごとに多いもの 3 つ、1 回あたり)

- pipe_rr: `sysfile:read` 9.3 µs × 9988、`file:pipe_read` 7.5 µs × 9988、`proc:wakeup` 3.2 µs × 19986
- futex_rr: `futex:futex` 3.5 µs × 11383、`sysfile:openat` 140.5 µs × 2、`vfs:lookup` 20.2 µs × 8
- unix_rr: `unix:recv` 12.0 µs × 10001、`sysfile:read` 7.0 µs × 10001、`unix:send` 5.0 µs × 10001
- tcp_rr: `inet:tcp_recv` 7.7 µs × 10001、`sysfile:read` 5.8 µs × 10001、`inet:tcp_send` 3.9 µs × 10001
- udp_rr: `inet:udp_recv` 7.2 µs × 10001、`inet:udp_send` 5.7 µs × 10001、`sysfile:read` 5.7 µs × 10001
- tcp_rr_epoll: `epoll:wait` 9.3 µs × 10001、`epoll:sleep` 10.1 µs × 8371、`epoll:collect` 3.9 µs × 18372
- unix_local: `unix:recv` 2.2 µs × 20000、`sysfile:read` 2.0 µs × 20000、`unix:send` 1.7 µs × 20000
- tcp_local: `sysfile:read` 1.4 µs × 20000、`inet:tcp_send` 1.3 µs × 20000、`inet:tcp_recv` 1.0 µs × 20000
- udp_local: `inet:udp_send` 1.6 µs × 20000、`sysfile:read` 1.4 µs × 20000、`inet:udp_recv` 1.1 µs × 20000
- unix_stream: `unix:recv` 48.0 µs × 572、`unix:send` 40.0 µs × 512、`sysfile:read` 32.2 µs × 572
- tcp_stream: `inet:tcp_send` 53.6 µs × 512、`inet:tcp_recv` 31.0 µs × 513、`sysfile:read` 27.5 µs × 513
- tcp_stream_4k: `inet:tcp_send` 8.0 µs × 2048、`inet:tcp_recv` 24.1 µs × 640、`sysfile:read` 14.0 µs × 640
- tcp_connect: `socket:connect` 9.0 µs × 1000、`inet:close` 4.2 µs × 2001、`inet:wake` 1.1 µs × 6001

## 直すところの候補

- unix_local: 前の記録 (8eee78f) の 1.16 倍の時間
- tcp_local: 前の記録 (8eee78f) の 1.12 倍の時間
- udp_local: 前の記録 (8eee78f) の 1.30 倍の時間
- unix_stream: Linux の 1.56 倍の時間

## これまで (同じ VM の記録。aios の値、かっこは aios / Linux)

| 記録 | pipe_rr | futex_rr | unix_rr | tcp_rr | udp_rr | tcp_rr_epoll | unix_local | tcp_local | udp_local | unix_stream | tcp_stream | tcp_stream_4k | tcp_connect |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-10-21 1de41f7 | 101 µs (0.84) | 15.9 µs (0.51) | 106 µs (0.46) | 89.0 µs (0.18) | 91.6 µs (0.25) | 128 µs (0.26) | 28.8 µs (0.88) | 23.6 µs (0.21) | 26.5 µs (0.33) | 277 MB/s (1.56) | 358 MB/s (0.85) | 240 MB/s (0.20) | 38.9 µs (0.04) |
| 2026-10-20 8eee78f | 109 µs (0.92) | - | 118 µs (0.47) | 103 µs (0.23) | 103 µs (0.27) | 142 µs (0.25) | 24.7 µs (0.77) | 21.1 µs (0.19) | 20.4 µs (0.25) | 278 MB/s (1.47) | 352 MB/s (0.95) | 230 MB/s (0.53) | 43.6 µs (0.05) |
| 2026-10-20 f9ae243 | 103 µs (0.87) | - | 111 µs (0.43) | 104 µs (0.22) | 103 µs (0.28) | 141 µs (0.28) | 22.5 µs (0.70) | 21.7 µs (0.20) | 21.7 µs (0.27) | 283 MB/s (1.42) | 325 MB/s (0.95) | 241 MB/s (0.15) | 166 µs (0.16) |
