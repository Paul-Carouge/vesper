# R2 corruption: hostile VST3 sidecar files and filter lines, with a real
# plugin installed. Each variant records the game for 12 s with a mic filter
# chain; the recording must succeed and Vesper must neither hang nor balloon.
# Env: VST (a .vst3 bundle).
. $LABROOT/lib.sh
. $LABROOT/audio_up.sh
audio_up
mkdir -p $HOME/.vst3 $L/cfg/vesper/vst3
cp -r $VST $HOME/.vst3/
B=$HOME/.vst3/$(basename $VST)
CID=${VST_CID:-6e33225254224a00aa69301af318797d}
start_game cor
base() { printf 'tracks=\ntrack=true|false|0|desktop|Desktop\ntrack=true|false|0|output:labmic|Mic\nfilter=true|gain\n%s\naudio_mix=true\nreplay=false\n' "$1" > $L/cfg/vesper/settings; }
conf() { printf "$1" > $L/cfg/vesper/vst3/1.conf; }
run() { # run <label>
  rm -f $L/log/cor.log
  $VESPER --record-game 12 > $L/log/cor.log 2>&1 &
  local v=$!
  sleep 9
  local rs=$(grep VmRSS /proc/$v/status | tr -s ' ' | cut -d' ' -f2)
  local th=$(ls /proc/$v/task | wc -l)
  local kids=$(pgrep -P $v -c)
  for i in $(seq 1 100); do kill -0 $v 2>/dev/null || break; sleep 0.2; done
  if kill -0 $v 2>/dev/null; then echo "$1: HUNG after 29 s"; kill -9 $v; wait $v; return; fi
  wait $v; local rc=$?
  local f=$(last_file)
  local fin=$(grep -c "Finished(Ok" $L/log/cor.log)
  echo "$1: rc=$rc finished=$fin rss=${rs}kB threads=$th host_children=$kids | $(grep -E "VST3|vst" $L/log/cor.log | sed -E 's/^[0-9T:.Z-]+ +//' | sort | uniq -c | head -3 | cut -c1-170 | tr '\n' ';')"
  rm -f "$f"
}
base "filter=true|vst3:id=1"
conf "path=$B\ncid=$CID\nname=Gain\ncomponent=\ncontroller=\n"; run "valid"
conf ""; run "empty conf"
conf "path=$B\ncid=zz\nname=Gain\n"; run "bad cid hex"
conf "path=$B\ncid=$CID\ncomponent=!!!notbase64\n"; run "garbage base64 state"
conf "path=$B\ncid=00000000000000000000000000000000\nname=X\ncomponent=\ncontroller=\n"; run "unknown class id"
conf "path=/dev/zero\ncid=$CID\nname=X\ncomponent=\ncontroller=\n"; run "path /dev/zero"
conf "path=/etc/passwd\ncid=$CID\nname=X\ncomponent=\ncontroller=\n"; run "path = a text file"
conf "path=/nonexistent/x.vst3\ncid=$CID\nname=X\ncomponent=\ncontroller=\n"; run "path missing"
conf "path=$B\ncid=$CID\nname=Gain\ncomponent=$(head -c 40000000 /dev/urandom | base64 -w0)\ncontroller=\n"; run "30 MB random component state"
conf "path=$B\ncid=$CID\nname=$(head -c 3000000 /dev/zero | tr '\0' 'A')\ncomponent=\ncontroller=\n"; run "3 MB plugin name"
rm -f $L/cfg/vesper/vst3/1.conf; mkdir $L/cfg/vesper/vst3/1.conf; run "conf is a directory"; rmdir $L/cfg/vesper/vst3/1.conf
ln -sf /etc/passwd $L/cfg/vesper/vst3/1.conf; run "conf is a symlink to /etc/passwd"; rm -f $L/cfg/vesper/vst3/1.conf
rm -f $L/cfg/vesper/vst3/1.conf; run "conf missing"
base "filter=true|vst3:id=4294967295"; run "id u32::MAX, no conf"
base "filter=true|vst3:id=-1"; run "negative id"
base "$(yes 'filter=true|vst3:id=1' | head -n 40)"; conf "path=$B\ncid=$CID\nname=Gain\ncomponent=\ncontroller=\n"; run "40 filters on the same id"
base "$(for i in $(seq 1 40); do echo "filter=true|vst3:id=$i"; cp -f $L/cfg/vesper/vst3/1.conf $L/cfg/vesper/vst3/$i.conf 2>/dev/null; done)"; run "40 different instances (hosts)"
stop_game
