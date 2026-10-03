// aios の Firefox の既定の設定 (/opt/c/lib/firefox/defaults/pref/aios-prefs.js)。about:config で変えられる
//
// 画面の更新は 1 秒に 30 回。aios はまだ GPU がなく (描くのは CPU)、カーネルは大きなロック 1 つなので、
// 既定の 60 回 (vsync) では間にあわないことがある。間にあわないと拡張機能のプロセスへの知らせ
// (1 秒に数百) がたまりつづけ、だんだん重くなってメモリが足りなくなる (4 CPU の QEMU で起きる)
pref("layout.frame_rate", 30);
