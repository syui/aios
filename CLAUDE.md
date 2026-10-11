# aios (unix ブランチ) で作業する Claude へ

- 開発は `unix` ブランチ。push は GitHub (`git push origin HEAD:unix`) と git.syui.ai (`bin/gitea.sh os`) の両方へ
- **push したら `bin/publish.sh`** を動かす (数分。CI を待つので aish の `run` の `bg` で)。CI がディスクの起動を
  確かめたら、リリース unix-latest を git.syui.ai にも置き、`base` / `aikernel` / `aiboot` のソースが変わっていれば
  作って確かめて ai/repo に送る。CI はこの 3 つをディスクに入れるだけなので、これをしないと入っている aios の
  `ap -Syu` に来ない (`bin/publish.sh -n` で、何をするかだけ見られる)
- `pkg/` のほかのパッケージ (`pkg/c/` など) を変えたときは、**先に** aish の `pkg_build` / `pkg_test` / `pkg_push` で
  ai/repo に送ってから `unix` に push する。CI はディスクを作るときに、base / aikernel / aiboot のほかは ai/repo から取る
- aios の中で確かめるのは aish の `vm` ツール (`kernel: "."` で作業中のカーネル、`snapshot: true`、`reboot`)。
  カーネルの入れかえと前のものに戻るところは `test/vm-boot.sh --new`、aish は `test/aish.sh`
