# パイプのテスト (test/vm.py run "$(cat test/pipe-check.sh)" で aios の中で)。カーネルのパイプ (大きなロックなしの
# read/write をふくむ) で、中身がこわれたり失われたりしないか、止まったまま進まなくならないか
f=0; c() { if eval "$1" >/dev/null 2>&1; then echo "ok $2"; else echo "FAIL $2"; f=$((f+1)); fi; }
a=$(seq 1 200000 | sha256sum | cut -c1-16)
c '[ "$(seq 1 200000 | cat | cat | cat | sha256sum | cut -c1-16)" = "$a" ]' chain
c '[ "$(seq 1 200000 | dd bs=7 2>/dev/null | dd bs=65536 2>/dev/null | sha256sum | cut -c1-16)" = "$a" ]' dd-sizes
b=$(seq 1 200000 | wc -c)
c '[ "$( (seq 1 50000 & seq 50001 100000 & seq 100001 150000 & seq 150001 200000 & wait) | wc -lc | tr -s " ")" = "$(echo " 200000 $b")" ]' four-writers
c '[ "$(head -c 3000000 /dev/zero | wc -c)" = 3000000 ]' big
c '[ "$(yes | head -n 100000 | wc -l)" = 100000 ]' epipe-yes
c '[ "$( (sleep 1; echo late) | cat)" = late ]' sleep-reader
c '[ "$(seq 1 1000 | (read x; echo $x))" = 1 ]' partial
c 'mkfifo /tmp/pf && (seq 1 20000 > /tmp/pf &) && [ "$(cat /tmp/pf | wc -l)" = 20000 ]; rm -f /tmp/pf' fifo
c '[ "$(sh -c "exec 3< <(seq 1 5); cat <&3" | wc -l)" = 5 ]' procsub
echo "pipe checks failed: $f"
