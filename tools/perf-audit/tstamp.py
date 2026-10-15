import sys, time
t0 = float(sys.argv[1])
for line in sys.stdin:
    print(f"{(time.time() - t0) * 1000:9.1f} ms  {line.rstrip()}", flush=True)
