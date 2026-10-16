# R2 calibration of r2_latency.sh's receiver: a smooth ffmpeg -re source
# (x264, 60 fps, 6 Mbit/s) to the same ffprobe listener. What it shows is the
# receiver's own jitter, to subtract from the Vesper numbers.
. $LABROOT/lib.sh
ffprobe -v error -listen 1 -fflags nobuffer -show_entries packet=stream_index,pts_time,size,flags -of csv rtmp://127.0.0.1:1935/live/test 2> $L/log/probe-cal.err | python3 -I $LABROOT/r2_arrival.py 3 > $L/cal.out &
P=$!
sleep 1
ffmpeg -loglevel error -re -f lavfi -i "testsrc2=size=1920x1080:rate=60" -t 25 -c:v libx264 -preset ultrafast -tune zerolatency -b:v 6M -maxrate 6M -bufsize 6M -g 120 -f flv rtmp://127.0.0.1:1935/live/test
wait $P
echo "== calibration"
cat $L/cal.out
