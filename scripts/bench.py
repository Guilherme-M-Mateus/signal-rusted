#!/usr/bin/env python3
"""Compare resource use of signal-rusted against Signal Desktop (the control). Linux only.

Launches a client, follows its whole process tree (Electron spawns many helpers) and samples
/proc: PSS (shared pages split fairly between processes), CPU time and process count.

  scripts/bench.py "target/release/signal-rusted" "flatpak run org.signal.Signal" --runs 3
"""
import argparse, os, shlex, signal, statistics, subprocess, time

HZ = os.sysconf("SC_CLK_TCK")


def tree(root):
    """root plus every descendant pid."""
    kids = {}
    for p in os.listdir("/proc"):
        if p.isdigit():
            try:
                ppid = int(open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()[1])
                kids.setdefault(ppid, []).append(int(p))
            except OSError:
                pass
    out, todo = [], [root]
    while todo:
        p = todo.pop()
        out.append(p)
        todo += kids.get(p, [])
    return out


def sample(root):
    """(pss_mib, cpu_seconds, processes) summed over the tree."""
    pss = cpu = n = 0
    for p in tree(root):
        try:
            for line in open(f"/proc/{p}/smaps_rollup"):
                if line.startswith("Pss:"):
                    pss += int(line.split()[1])
            f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            cpu += (int(f[11]) + int(f[12])) / HZ  # utime + stime
            n += 1
        except (OSError, IndexError):
            pass
    return pss / 1024, cpu, n


def run(cmd, settle, idle):
    proc = subprocess.Popen(shlex.split(cmd), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True)
    t0, last_cpu, startup = time.monotonic(), 0.0, None
    # Startup = first full second after launch where the tree uses under 5% of one core.
    while time.monotonic() - t0 < settle:
        time.sleep(1)
        _, c, _ = sample(proc.pid)
        if startup is None and c - last_cpu < 0.05 and c > 0.2:
            startup = time.monotonic() - t0 - 1
        last_cpu = c
    pss0, cpu0, n = sample(proc.pid)
    peak = pss0
    for _ in range(idle):
        time.sleep(1)
        peak = max(peak, sample(proc.pid)[0])
    pss1, cpu1, n = sample(proc.pid)
    os.killpg(proc.pid, signal.SIGTERM)
    try:
        proc.wait(10)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
    time.sleep(3)  # let helpers exit before the next run
    return dict(startup_s=startup or float("nan"), startup_cpu_s=cpu0, pss_mib=pss1, peak_pss_mib=peak,
                idle_cpu_pct=(cpu1 - cpu0) / idle * 100, processes=n)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("commands", nargs="+")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--settle", type=int, default=30, help="seconds before idle sampling starts")
    ap.add_argument("--idle", type=int, default=30, help="idle sampling window, seconds")
    a = ap.parse_args()
    for cmd in a.commands:
        runs = [run(cmd, a.settle, a.idle) for _ in range(a.runs)]
        print(f"\n{cmd}  (median of {a.runs})")
        for k in runs[0]:
            print(f"  {k:15} {statistics.median(r[k] for r in runs):8.1f}   runs: "
                  + ", ".join(f"{r[k]:.1f}" for r in runs))


if __name__ == "__main__":
    main()
