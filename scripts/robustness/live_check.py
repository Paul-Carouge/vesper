"""Checks a received live stream (FLV or MPEG-TS) with ffprobe.

usage: live_check.py <file> [expect_codec] [expect_height] [expect_fps] [lossy]

`lossy`: frames were dropped on purpose (congestion), keyframes forced.

Prints the streams, the keyframe interval, the A/V span and the largest
video gap, and exits non-zero when one is off.
"""
import json
import subprocess
import sys


def probe(path, *args):
    out = subprocess.run(
        ["ffprobe", "-v", "error", "-of", "json", *args, path],
        capture_output=True, text=True, check=True,
    ).stdout
    return json.loads(out)


def main():
    path = sys.argv[1]
    want_codec = sys.argv[2] if len(sys.argv) > 2 else None
    want_height = int(sys.argv[3]) if len(sys.argv) > 3 else None
    want_fps = int(sys.argv[4]) if len(sys.argv) > 4 else None
    ok = True
    streams = probe(path, "-show_streams")["streams"]
    video = [s for s in streams if s["codec_type"] == "video"]
    audio = [s for s in streams if s["codec_type"] == "audio"]
    for s in streams:
        extra = (f"{s.get('width')}x{s.get('height')} {s.get('avg_frame_rate')}"
                 if s["codec_type"] == "video"
                 else f"{s.get('sample_rate')} Hz {s.get('channels')} ch")
        print(f"stream {s['index']}: {s['codec_type']} {s['codec_name']} {extra}")
    if not video:
        print("FAIL: no video")
        return 1
    v = video[0]
    if want_codec and v["codec_name"] != want_codec:
        print(f"FAIL: video codec {v['codec_name']}, want {want_codec}")
        ok = False
    if want_height and v["height"] != want_height:
        print(f"FAIL: height {v['height']}, want {want_height}")
        ok = False
    if not audio or audio[0]["codec_name"] != "aac":
        print("FAIL: no AAC audio")
        ok = False

    packets = probe(path, "-show_packets", "-show_entries",
                    "packet=stream_index,pts_time,flags")["packets"]
    vp = [p for p in packets if p["stream_index"] == v["index"] and "pts_time" in p]
    ap = [p for p in packets if audio and p["stream_index"] == audio[0]["index"]
          and "pts_time" in p]
    vt = [float(p["pts_time"]) for p in vp]
    keys = [float(p["pts_time"]) for p in vp if "K" in p["flags"]]
    if not vp or "K" not in vp[0]["flags"]:
        print("FAIL: does not start on a keyframe")
        ok = False
    gaps = [b - a for a, b in zip(keys, keys[1:])]
    print(f"video: {len(vt)} frames over {vt[-1] - vt[0]:.2f} s "
          f"({(len(vt) - 1) / max(vt[-1] - vt[0], 1e-9):.2f} fps), "
          f"{len(keys)} keyframes, intervals {sorted(set(round(g, 2) for g in gaps))}")
    odd = [g for g in gaps if abs(g - 2.0) > 0.05]
    # Keyframes forced by a reconnection or a congestion are allowed: report.
    lossy = len(sys.argv) > 5 and sys.argv[5] == "lossy"
    if gaps and len(odd) > max(2, len(gaps) // 5) and not lossy:
        print(f"FAIL: keyframe interval not 2 s: {gaps}")
        ok = False
    frame_gaps = [b - a for a, b in zip(vt, vt[1:])]
    print(f"largest video gap {max(frame_gaps) * 1000:.0f} ms")
    if want_fps:
        fps = (len(vt) - 1) / (vt[-1] - vt[0])
        if abs(fps - want_fps) > 0.5:
            print(f"FAIL: {fps:.2f} fps, want {want_fps}")
            ok = False
    if ap:
        at = [float(p["pts_time"]) for p in ap]
        print(f"audio: {len(at)} packets, {at[0]:.3f} .. {at[-1]:.3f} s; "
              f"video {vt[0]:.3f} .. {vt[-1]:.3f} s; "
              f"start offset {(at[0] - vt[0]) * 1000:+.0f} ms, "
              f"end offset {(at[-1] - vt[-1]) * 1000:+.0f} ms")
        if abs(at[0] - vt[0]) > 0.15 or abs(at[-1] - vt[-1]) > 0.5:
            print("FAIL: audio and video do not cover the same time")
            ok = False
        audio_gaps = [b - a for a, b in zip(at, at[1:])]
        print(f"largest audio gap {max(audio_gaps) * 1000:.0f} ms")
    print("OK" if ok else "FAILED")
    return 0 if ok else 1


sys.exit(main())
