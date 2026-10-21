# ループバックの速さ (TCP、UDP、AF_UNIX、パイプ)

aish の `bench` (kind net、linux、record) が作る。もとは bench/net.jsonl (1 行 1 回の記録)。測るのは test/netbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-21T02:19:15Z の 6b0a13c (r574)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| pipe_rr | 81.8 µs | 123 µs | 0.67 | 0.81 | sysfile 41%, file 30%, proc 28% | パイプで 1 バイトの往復 (2 つのプロセス。くらべるための土台: プロセスの切りかえと起こし) |
| futex_rr | 14.8 µs | 36.3 µs | 0.41 | 0.93 | futex 99%, sysfile 1%, vfs 1% | 2 つのスレッドが futex で交互に起こしあう往復 (スレッドの切りかえと futex の起こしだけ) |
| unix_rr | 80.3 µs | 239 µs | 0.34 | 0.76 | unix 55%, sysfile 37%, proc 8% | AF_UNIX の stream で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr | 87.3 µs | 494 µs | 0.18 | 0.98 | inet 58%, sysfile 34%, proc 8% | TCP (127.0.0.1、TCP_NODELAY) で 1 バイトの往復 (2 つのプロセス) |
| udp_rr | 84.3 µs | 375 µs | 0.23 | 0.92 | inet 56%, sysfile 34%, proc 10% | UDP (つないだもの) で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr_epoll | 113 µs | 509 µs | 0.22 | 0.89 | epoll 57%, inet 25%, sysfile 15% | TCP の往復で、読む前に epoll_wait で待つ (サーバーのよくある形) |
| unix_local | 20.9 µs | 32.7 µs | 0.64 | 0.73 | unix 51%, sysfile 40%, proc 9% | AF_UNIX の往復を 1 つのプロセスで (切りかえなし: ソケットの道だけ) |
| tcp_local | 24.5 µs | 115 µs | 0.21 | 1.04 | inet 57%, sysfile 34%, proc 9% | TCP の往復を 1 つのプロセスで |
| udp_local | 18.9 µs | 83.9 µs | 0.23 | 0.71 | inet 54%, sysfile 36%, proc 10% | UDP の往復を 1 つのプロセスで |
| unix_stream | 437 MB/s | 390 MB/s | 0.89 | 0.63 | unix 63%, sysfile 28%, proc 8% | AF_UNIX に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream | 485 MB/s | 252 MB/s | 0.52 | 0.74 | inet 76%, sysfile 21%, proc 2% | TCP に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream_4k | 293 MB/s | 141 MB/s | 0.48 | 0.82 | inet 71%, sysfile 20%, proc 7% | TCP に 4 KiB ずつ書いて、別のプロセスが読む |
| tcp_connect | 41.3 µs | 919 µs | 0.04 | 1.06 | inet 47%, socket 33%, sysfile 11% | connect、accept、両方を閉じる (1 つのつながり) |

## CPU を使っている区間 (aios、op ごとに多いもの 3 つ、1 回あたり)

- pipe_rr: `sysfile:read` 8.8 µs × 9990、`file:pipe_read` 6.5 µs × 9990、`proc:wakeup` 3.1 µs × 19990
- futex_rr: `futex:futex` 3.0 µs × 14527、`sysfile:openat` 113.0 µs × 2、`vfs:lookup` 17.5 µs × 8
- unix_rr: `unix:recv` 8.7 µs × 10001、`sysfile:read` 7.9 µs × 10001、`unix:send` 4.5 µs × 10001
- tcp_rr: `inet:tcp_recv` 7.8 µs × 10001、`sysfile:read` 6.9 µs × 10001、`inet:tcp_send` 4.1 µs × 10001
- udp_rr: `sysfile:read` 10.0 µs × 10001、`inet:udp_recv` 9.6 µs × 10001、`inet:udp_send` 9.1 µs × 10001
- tcp_rr_epoll: `epoll:wait` 8.9 µs × 10001、`epoll:collect` 4.0 µs × 17648、`epoll:sleep` 8.8 µs × 7647
- unix_local: `sysfile:read` 2.2 µs × 20000、`unix:send` 1.8 µs × 20000、`unix:recv` 1.2 µs × 20000
- tcp_local: `sysfile:read` 1.4 µs × 20000、`inet:tcp_send` 1.3 µs × 20000、`inet:tcp_recv` 1.0 µs × 20000
- udp_local: `inet:udp_send` 1.8 µs × 20000、`sysfile:read` 1.5 µs × 20000、`inet:wake` 1.0 µs × 20000
- unix_stream: `unix:send` 22.2 µs × 512、`unix:recv` 19.1 µs × 570、`sysfile:read` 16.0 µs × 570
- tcp_stream: `inet:tcp_send` 39.8 µs × 512、`inet:tcp_recv` 20.7 µs × 513、`sysfile:read` 15.5 µs × 513
- tcp_stream_4k: `inet:tcp_send` 2.9 µs × 2048、`inet:tcp_recv` 36.4 µs × 131、`sysfile:read` 15.5 µs × 131
- tcp_connect: `socket:connect` 8.3 µs × 1000、`inet:close` 3.8 µs × 2001、`inet:wake` 0.9 µs × 6001

## これまで (同じ VM の記録。aios の値、かっこは aios / Linux)

| 記録 | pipe_rr | futex_rr | unix_rr | tcp_rr | udp_rr | tcp_rr_epoll | unix_local | tcp_local | udp_local | unix_stream | tcp_stream | tcp_stream_4k | tcp_connect |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-10-21 6b0a13c | 81.8 µs (0.67) | 14.8 µs (0.41) | 80.3 µs (0.34) | 87.3 µs (0.18) | 84.3 µs (0.23) | 113 µs (0.22) | 20.9 µs (0.64) | 24.5 µs (0.21) | 18.9 µs (0.23) | 437 MB/s (0.89) | 485 MB/s (0.52) | 293 MB/s (0.48) | 41.3 µs (0.04) |
| 2026-10-21 1de41f7 | 101 µs (0.84) | 15.9 µs (0.51) | 106 µs (0.46) | 89.0 µs (0.18) | 91.6 µs (0.25) | 128 µs (0.26) | 28.8 µs (0.88) | 23.6 µs (0.21) | 26.5 µs (0.33) | 277 MB/s (1.56) | 358 MB/s (0.85) | 240 MB/s (0.20) | 38.9 µs (0.04) |
| 2026-10-20 8eee78f | 109 µs (0.92) | - | 118 µs (0.47) | 103 µs (0.23) | 103 µs (0.27) | 142 µs (0.25) | 24.7 µs (0.77) | 21.1 µs (0.19) | 20.4 µs (0.25) | 278 MB/s (1.47) | 352 MB/s (0.95) | 230 MB/s (0.53) | 43.6 µs (0.05) |
| 2026-10-20 f9ae243 | 103 µs (0.87) | - | 111 µs (0.43) | 104 µs (0.22) | 103 µs (0.28) | 141 µs (0.28) | 22.5 µs (0.70) | 21.7 µs (0.20) | 21.7 µs (0.27) | 283 MB/s (1.42) | 325 MB/s (0.95) | 241 MB/s (0.15) | 166 µs (0.16) |
