# Sourced inside `unshare -Urmp`: a private session (uid 0 inside, so the
# capture socket is /tmp/vesper-0, never the real /tmp/vesper-1000).
# LAB=<name> picks an instance dir so scenarios can run side by side.
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
R=$(cd "$HERE/../.." && pwd)
LABROOT=$HERE
DATA=${DATA:-/tmp/vesper-lab-$(id -u)}
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
# Never the session's display, nor its X cookie.
unset WAYLAND_DISPLAY DISPLAY XAUTHORITY
export VESPER=$R/target/debug/vesper
cat > $L/layers/vesper_capture.json <<J
{"file_format_version":"1.2.1","layer":{"name":"VK_LAYER_VESPER_capture","type":"GLOBAL","library_path":"$R/target/debug/libvesper_vklayer.so","library_arch":"64","api_version":"1.4.0","implementation_version":"1","description":"lab","disable_environment":{"VESPER_CAPTURE_DISABLE":"1"}}}
J
# 32-bit games: the i686 debug build, if built
# (cargo build -p vesper-vklayer -p vesper-glcapture --target i686-unknown-linux-gnu).
I686=$R/target/i686-unknown-linux-gnu/debug
cat > $L/layers/vesper_capture_32.json <<J
{"file_format_version":"1.2.1","layer":{"name":"VK_LAYER_VESPER_capture_32","type":"GLOBAL","library_path":"$I686/libvesper_vklayer.so","library_arch":"32","api_version":"1.4.0","implementation_version":"1","description":"lab","disable_environment":{"VESPER_CAPTURE_DISABLE":"1"}}}
J
export VK_ADD_IMPLICIT_LAYER_PATH=$L/layers
# OpenGL games: "$VESPER_GL game", the real wrapper in a <prefix>/bin,
# lib/vesper, lib32/vesper layout of the debug builds.
mkdir -p $L/glprefix/bin $L/glprefix/lib/vesper $L/glprefix/lib32/vesper
cp $R/crates/vesper-glcapture/vesper-gl $L/glprefix/bin/
ln -sf $R/target/debug/libvesper_glcapture.so $L/glprefix/lib/vesper/
ln -sf $I686/libvesper_glcapture.so $L/glprefix/lib32/vesper/
export VESPER_GL=$L/glprefix/bin/vesper-gl
