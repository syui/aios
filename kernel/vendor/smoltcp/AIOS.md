# smoltcp 0.12.0 (aios の写し)

crates.io の smoltcp 0.12.0 (0BSD) の `src/`、`build.rs`、`LICENSE-0BSD.txt` と、examples・benches・dev-dependencies・
profile を外した `Cargo.toml`。ルートの `Cargo.toml` の `[patch.crates-io]` がこれを使う (workspace の exclude にも)。

変えたところ (`grep -rn aios src/` で見られる):

- 近所さがし (ARP と IPv6 の NS) のだまる時間をあて先ごとに (`src/iface/neighbor.rs` の `silent_until` と
  `limit_rate`、`src/iface/interface/mod.rs` の呼ぶところ)。もとは表全体で 1 つで、1 つ問い合わせると、どのあて先にも
  1 秒は問い合わせなかった。答えのないあて先へ送りつづけるソケットがあると、そのソケットが 1 秒ごとに問い合わせなおす
  たびに、ほかのあて先はずっと問い合わせてもらえなかった (Linux は neighbour ごとの retrans_time)。0.14 も同じ。
  test/ndsilence.c (todo の smoltcp-nd-silence)

smoltcp を上げるときは、新しい版の写しにこれらをあてなおす。
