# Sourced inside `unshare -Urmp`: a private session (uid 0 inside, so the
# capture socket is /tmp/vesper-0, never the real /tmp/vesper-1000).
# LAB=<name> picks an instance dir so scenarios can run side by side.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
R=$(cd "$HERE/../.." && pwd)
LABROOT=$HERE
DATA=${VESPER_LAB_DATA:-/tmp/vesper-lab-$(id -u)}
L=$DATA/${LAB:-main}
export HERE R L LABROOT
export XDG_RUNTIME_DIR=/tmp/lab-${LAB:-main}-run XDG_CONFIG_HOME=$L/cfg XDG_DATA_HOME=$L/data XDG_CACHE_HOME=$L/cache HOME=$L/home
mkdir -p $XDG_RUNTIME_DIR $L/run $L/cfg $L/data $L/cache $L/home $L/vid $L/layers $L/log $L/cfg/wireplumber/wireplumber.conf.d
chmod 700 $XDG_RUNTIME_DIR
echo "XDG_VIDEOS_DIR=\"$L/vid\"" > $L/cfg/user-dirs.dirs
# Never touch the real audio/video hardware from the private WirePlumber.
cat > $L/cfg/wireplumber/wireplumber.conf.d/90-lab.conf <<J
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.libcamera = disabled
    monitor.v4l2 = disabled
  }
}
J
mkdir -p $L/cfg/pipewire/pipewire.conf.d
cat > $L/cfg/pipewire/pipewire.conf.d/10-lab.conf <<J
context.objects = [
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = labsink node.description = labsink media.class = Audio/Sink object.linked = true audio.position = [ FL FR ] } }
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = labsink2 node.description = labsink2 media.class = Audio/Sink object.linked = true audio.position = [ FL FR ] } }
]
J
unset WAYLAND_DISPLAY DISPLAY
export VESPER=$R/target/debug/vesper
cat > $L/layers/vesper_capture.json <<J
{"file_format_version":"1.2.0","layer":{"name":"VK_LAYER_VESPER_capture","type":"GLOBAL","library_path":"$R/target/debug/libvesper_vklayer.so","api_version":"1.4.0","implementation_version":"1","description":"lab","disable_environment":{"VESPER_CAPTURE_DISABLE":"1"}}}
J
export VK_ADD_IMPLICIT_LAYER_PATH=$L/layers
