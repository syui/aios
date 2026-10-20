# ループバックの速さ (TCP、UDP、AF_UNIX、パイプ)

aish の `bench` (kind net、linux、record) が作る。もとは bench/net.jsonl (1 行 1 回の記録)。測るのは test/netbench.c で、aios と
Linux の VM で同じプログラム (同じ libc) を動かす。時間の比は aios / Linux (1 より小さければ aios が速い)。part は aios の
/proc/ai/prof で CPU を使った時間の多いもの (眠って待った時間はのぞく)。VM は QEMU の TCG (x86 の上で aarch64 をまねる)
なので、本物の機械とは比がちがうことがある

いちばん新しい記録: 2026-10-20T23:17:57Z の f9ae243 (r564)。VM: cortex-a72 × 1、2G、tcg (ホスト: Intel(R) Xeon(R) Processor @ 2.10GHz × 4)

| op | aios | Linux 7.3 | aios / Linux | 前の記録から | part (aios) | 中身 |
|---|---|---|---|---|---|---|
| pipe_rr | 103 µs | 118 µs | 0.87 | - | sysfile 40%, file 34%, proc 26% | パイプで 1 バイトの往復 (2 つのプロセス。くらべるための土台: プロセスの切りかえと起こし) |
| unix_rr | 111 µs | 261 µs | 0.43 | - | unix 63%, sysfile 32%, proc 6% | AF_UNIX の stream で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr | 104 µs | 470 µs | 0.22 | - | inet 58%, sysfile 35%, proc 6% | TCP (127.0.0.1、TCP_NODELAY) で 1 バイトの往復 (2 つのプロセス) |
| udp_rr | 103 µs | 373 µs | 0.28 | - | inet 62%, sysfile 32%, proc 6% | UDP (つないだもの) で 1 バイトの往復 (2 つのプロセス) |
| tcp_rr_epoll | 141 µs | 506 µs | 0.28 | - | epoll 54%, inet 27%, sysfile 16% | TCP の往復で、読む前に epoll_wait で待つ (サーバーのよくある形) |
| unix_local | 22.5 µs | 31.9 µs | 0.70 | - | unix 60%, sysfile 32%, proc 8% | AF_UNIX の往復を 1 つのプロセスで (切りかえなし: ソケットの道だけ) |
| tcp_local | 21.7 µs | 108 µs | 0.20 | - | inet 58%, sysfile 33%, proc 9% | TCP の往復を 1 つのプロセスで |
| udp_local | 21.7 µs | 82.1 µs | 0.27 | - | inet 55%, sysfile 35%, proc 8% | UDP の往復を 1 つのプロセスで |
| unix_stream | 283 MB/s | 402 MB/s | 1.42 | - | unix 70%, sysfile 25%, proc 4% | AF_UNIX に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream | 325 MB/s | 308 MB/s | 0.95 | - | inet 66%, sysfile 30%, proc 2% | TCP に 64 KiB ずつ書いて、別のプロセスが読む |
| tcp_stream_4k | 241 MB/s | 36 MB/s | 0.15 | - | inet 72%, sysfile 20%, proc 5% | TCP に 4 KiB ずつ書いて、別のプロセスが読む |
| tcp_connect | 166 µs | 1015 µs | 0.16 | - | inet 66%, socket 29%, proc 3% | connect、accept、両方を閉じる (1 つのつながり) |

## CPU を使っている区間 (aios、op ごとに多いもの 3 つ、1 回あたり)

- pipe_rr: `sysfile:read` 8.8 µs × 9994、`file:pipe_read` 7.6 µs × 9994、`proc:wakeup` 3.0 µs × 20001
- unix_rr: `unix:recv` 10.1 µs × 10001、`sysfile:read` 6.2 µs × 10001、`unix:send` 3.1 µs × 10001
- tcp_rr: `inet:tcp_recv` 8.7 µs × 10001、`sysfile:read` 7.2 µs × 10018、`inet:tcp_send` 3.6 µs × 10001
- udp_rr: `inet:udp_recv` 7.6 µs × 10001、`inet:udp_send` 7.0 µs × 10001、`sysfile:read` 6.6 µs × 10001
- tcp_rr_epoll: `epoll:wait` 18.8 µs × 10001、`sysfile:read` 4.2 µs × 10001、`inet:tcp_send` 3.1 µs × 10001
- unix_local: `unix:recv` 2.0 µs × 20000、`sysfile:read` 1.6 µs × 20000、`unix:send` 1.4 µs × 20000
- tcp_local: `sysfile:read` 1.6 µs × 20000、`inet:tcp_send` 1.5 µs × 20000、`inet:tcp_recv` 1.2 µs × 20000
- udp_local: `inet:udp_send` 2.2 µs × 20000、`sysfile:read` 1.7 µs × 20017、`inet:wake` 1.0 µs × 20000
- unix_stream: `unix:recv` 43.2 µs × 571、`unix:send` 38.8 µs × 512、`sysfile:read` 26.6 µs × 571
- tcp_stream: `inet:tcp_send` 34.6 µs × 512、`inet:tcp_recv` 30.1 µs × 513、`sysfile:read` 29.4 µs × 513
- tcp_stream_4k: `inet:tcp_send` 4.1 µs × 2048、`inet:tcp_recv` 58.7 µs × 131、`sysfile:read` 27.1 µs × 131
- tcp_connect: `inet:close` 62.7 µs × 2001、`socket:socket` 70.3 µs × 1001、`inet:syn` 60.6 µs × 1000

## 直すところの候補

- unix_stream: Linux の 1.42 倍の時間
