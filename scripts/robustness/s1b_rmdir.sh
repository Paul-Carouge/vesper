# S1b: the output folder is deleted mid-recording; then: unwritable folder, folder replaced by a file
. $LABROOT/lib.sh
start_game s1b
echo "=== folder removed mid-recording"
$VESPER --record-game 14 > $L/log/vesper-a.log 2>&1 &
V=$!
sleep 9
rm -rf $L/vid/Vesper
wait $V
grep -E "Finished|Stopped" $L/log/vesper-a.log | cut -c1-260
echo "files left: $(ls $L/vid/Vesper 2>&1 | head -2)"
echo "=== unwritable folder (root in the namespace ignores modes: use a read-only mount)"
mkdir -p $L/ro
mount -t tmpfs tmpfs $L/ro
mkdir -p $L/ro/Vesper
mount -o remount,ro,bind $L/ro $L/ro 2>/dev/null || mount -o remount,ro $L/ro
echo "XDG_VIDEOS_DIR=\"$L/ro\"" > $L/cfg/user-dirs.dirs
$VESPER --record-game 6 > $L/log/vesper-b.log 2>&1
grep -E "Finished|Stopped|panick" $L/log/vesper-b.log | cut -c1-260
echo "=== output folder is a file"
echo x > $L/vid2
echo "XDG_VIDEOS_DIR=\"$L/vid2\"" > $L/cfg/user-dirs.dirs
$VESPER --record-game 6 > $L/log/vesper-c.log 2>&1
grep -E "Finished|Stopped|panick" $L/log/vesper-c.log | cut -c1-260
stop_game
