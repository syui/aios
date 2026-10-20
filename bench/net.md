# ループバックの速さ (TCP、UDP、AF_UNIX、パイプ)

aish の `bench` (kind net、linux、record) が作る。もとは bench/net.jsonl (1 行 1 回の記録)。測るのは test/netbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-20T23:49:42Z の 8eee78f (r566)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| pipe_rr | 109 µs | 118 µs | 0.92 | 1.05 | sysfile 40%, proc 32%, file 28% | パイプで 1 バイトの往復 (2 つのプロセス。くらべるための土台: プロセスの切りかえと起こし) |
| unix_rr | 118 µs | 249 µs | 0.47 | 1.06 | unix 60%, sysfile 31%, proc 10% | AF_UNIX の stream で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr | 103 µs | 456 µs | 0.23 | 0.99 | inet 58%, sysfile 31%, proc 10% | TCP (127.0.0.1、TCP_NODELAY) で 1 バイトの往復 (2 つのプロセス) |
| udp_rr | 103 µs | 386 µs | 0.27 | 1.00 | inet 59%, sysfile 30%, proc 11% | UDP (つないだもの) で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr_epoll | 142 µs | 577 µs | 0.25 | 1.01 | epoll 51%, inet 28%, sysfile 15% | TCP の往復で、読む前に epoll_wait で待つ (サーバーのよくある形) |
| unix_local | 24.7 µs | 32.0 µs | 0.77 | 1.10 | unix 57%, sysfile 31%, proc 12% | AF_UNIX の往復を 1 つのプロセスで (切りかえなし: ソケットの道だけ) |
| tcp_local | 21.1 µs | 111 µs | 0.19 | 0.97 | inet 55%, sysfile 33%, proc 12% | TCP の往復を 1 つのプロセスで |
| udp_local | 20.4 µs | 82.3 µs | 0.25 | 0.94 | inet 53%, sysfile 33%, proc 14% | UDP の往復を 1 つのプロセスで |
| unix_stream | 278 MB/s | 410 MB/s | 1.47 | 1.02 | unix 68%, sysfile 26%, proc 5% | AF_UNIX に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream | 352 MB/s | 335 MB/s | 0.95 | 0.92 | inet 72%, sysfile 25%, proc 2% | TCP に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream_4k | 230 MB/s | 123 MB/s | 0.53 | 1.05 | inet 71%, sysfile 20%, proc 7% | TCP に 4 KiB ずつ書いて、別のプロセスが読む |
| tcp_connect | 43.6 µs | 907 µs | 0.05 | 0.26 | inet 46%, socket 32%, proc 13% | connect、accept、両方を閉じる (1 つのつながり) |

## CPU を使っている区間 (aios、op ごとに多いもの 3 つ、1 回あたり)

- pipe_rr: `sysfile:read` 8.6 µs × 9990、`proc:wakeup` 3.5 µs × 19993、`file:pipe_read` 6.1 µs × 9990
- unix_rr: `unix:recv` 9.0 µs × 10001、`sysfile:read` 6.2 µs × 10001、`unix:send` 3.2 µs × 10001
- tcp_rr: `inet:tcp_recv` 7.5 µs × 10001、`sysfile:read` 5.8 µs × 10001、`inet:tcp_send` 3.4 µs × 10001
- udp_rr: `inet:udp_recv` 6.3 µs × 10001、`sysfile:read` 5.3 µs × 10001、`inet:udp_send` 5.2 µs × 10001
- tcp_rr_epoll: `epoll:wait` 19.4 µs × 10001、`sysfile:read` 4.0 µs × 10001、`inet:tcp_recv` 3.5 µs × 10001
- unix_local: `unix:recv` 1.8 µs × 20000、`sysfile:read` 1.5 µs × 20000、`unix:send` 1.4 µs × 20000
- tcp_local: `sysfile:read` 1.6 µs × 20000、`inet:tcp_send` 1.5 µs × 20000、`inet:tcp_recv` 1.2 µs × 20000
- udp_local: `inet:udp_send` 1.8 µs × 20000、`sysfile:read` 1.4 µs × 20000、`inet:wake` 1.1 µs × 20000
- unix_stream: `unix:recv` 45.0 µs × 564、`unix:send` 36.2 µs × 512、`sysfile:read` 28.8 µs × 564
- tcp_stream: `inet:tcp_send` 54.2 µs × 512、`inet:tcp_recv` 31.1 µs × 513、`sysfile:read` 28.2 µs × 513
- tcp_stream_4k: `inet:tcp_recv` 62.8 µs × 130、`inet:tcp_send` 4.0 µs × 2048、`sysfile:read` 28.0 µs × 130
- tcp_connect: `socket:connect` 8.0 µs × 1000、`inet:close` 3.6 µs × 2001、`inet:wake` 0.9 µs × 6001

## 直すところの候補

- unix_stream: Linux の 1.47 倍の時間

## これまで (同じ VM の記録。aios の値、かっこは aios / Linux)

| 記録 | pipe_rr | unix_rr | tcp_rr | udp_rr | tcp_rr_epoll | unix_local | tcp_local | udp_local | unix_stream | tcp_stream | tcp_stream_4k | tcp_connect |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-10-20 8eee78f | 109 µs (0.92) | 118 µs (0.47) | 103 µs (0.23) | 103 µs (0.27) | 142 µs (0.25) | 24.7 µs (0.77) | 21.1 µs (0.19) | 20.4 µs (0.25) | 278 MB/s (1.47) | 352 MB/s (0.95) | 230 MB/s (0.53) | 43.6 µs (0.05) |
| 2026-10-20 f9ae243 | 103 µs (0.87) | 111 µs (0.43) | 104 µs (0.22) | 103 µs (0.28) | 141 µs (0.28) | 22.5 µs (0.70) | 21.7 µs (0.20) | 21.7 µs (0.27) | 283 MB/s (1.42) | 325 MB/s (0.95) | 241 MB/s (0.15) | 166 µs (0.16) |
