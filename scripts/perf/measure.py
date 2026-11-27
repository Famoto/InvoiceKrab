#!/usr/bin/env python3
"""Measures the numbers in docs/PERFORMANCE.md. Linux only, stdlib only.

    cargo build --release -p einvoice-interfaces
    cargo build --release --target x86_64-unknown-linux-musl -p einvoice-interfaces
    python3 -I scripts/perf/measure.py OUT_DIR [GROUP ...]

GROUPs (default: all, in this order): fixtures throughput scaling cli memory
concurrency overload mixed. Each writes OUT_DIR/<group>.json; `fixtures`
writes OUT_DIR/fixtures/<format>-<lines>.xml, which the others read.

Needs `oha`, `hyperfine` and `taskset` on PATH. The server is pinned to the
first physical cores and the load generator to the remaining ones, so they
never share a core (except where a group says otherwise). Physical cores are
read from sysfs; SMT siblings of server cores are left idle.
"""

import json
import os
import re
import socket
import subprocess
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GLIBC = ROOT / "target/release"
MUSL = ROOT / "target/x86_64-unknown-linux-musl/release"
SEED = ROOT / "testfiles/en16931-full-ubl.xml"

FORMATS = {  # short name -> CLI/server format name
    "ubl": "ubl-invoice",
    "xrechnung": "xrechnung-invoice",
    "peppol": "peppol-bis-billing",
    "facturx": "facturx-invoice",
    "fatturapa": "fatturapa",
}
SIZES = [3, 100, 1_000, 10_000, 100_000]  # invoice lines
SMALL, LARGE = 3, 10_000


def physical_cpus():
    """One logical CPU per physical core, in core order."""
    seen, out = set(), []
    for cpu in sorted(Path("/sys/devices/system/cpu").glob("cpu[0-9]*"), key=lambda p: int(p.name[3:])):
        sib = (cpu / "topology/thread_siblings_list").read_text().strip()
        if sib not in seen:
            seen.add(sib)
            out.append(int(cpu.name[3:]))
    return out


def siblings(cpu):
    text = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list").read_text()
    out = []
    for part in text.strip().split(","):
        a, _, b = part.partition("-")
        out.extend(range(int(a), int(b or a) + 1))
    return out


def split_cpus(n):
    """(server cpus, load cpus): n physical cores for the server, every
    thread of the other physical cores for the load generator. When the
    server takes every core, the load generator gets the server cores' SMT
    siblings (and shares physical cores with it)."""
    phys = physical_cpus()
    server = phys[:n]
    load = [c for p in phys[n:] for c in siblings(p)]
    if not load:
        load = [c for p in server for c in siblings(p) if c != p]
    return ",".join(map(str, server)), ",".join(map(str, load))


def fixture(out, fmt, lines):
    return out / "fixtures" / f"{fmt}-{lines}.xml"


# ---------------------------------------------------------------- fixtures


def make_ubl(lines):
    """The EN 16931 example with its three invoice lines repeated, in turn,
    to `lines` lines, each with a fresh line ID (BT-126)."""
    text = SEED.read_text()
    blocks = list(re.finditer(r"[ \t]*<cac:InvoiceLine>.*?</cac:InvoiceLine>\n", text, re.S))
    head, tail = text[: blocks[0].start()], text[blocks[-1].end():]
    body = []
    for i in range(lines):
        block = blocks[i % len(blocks)].group(0)
        body.append(re.sub(r"<cbc:ID>[^<]*</cbc:ID>", f"<cbc:ID>{i + 1}</cbc:ID>", block, count=1))
    return head + "".join(body) + tail


def g_fixtures(out):
    d = out / "fixtures"
    d.mkdir(parents=True, exist_ok=True)
    sizes = {}
    for lines in SIZES:
        ubl = fixture(out, "ubl", lines)
        ubl.write_text(make_ubl(lines))
        for fmt, name in FORMATS.items():
            if fmt != "ubl":
                subprocess.run(
                    [GLIBC / "krab-cli", ubl, name, "--out", fixture(out, fmt, lines)],
                    check=True, stderr=subprocess.DEVNULL,
                )
            sizes[f"{fmt}-{lines}"] = fixture(out, fmt, lines).stat().st_size
    return sizes


# ------------------------------------------------------------------ server


class Server:
    def __init__(self, workers, cpus, binary=GLIBC, **env):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        e = dict(os.environ, KRAB_ADDR=f"127.0.0.1:{self.port}", KRAB_WORKERS=str(workers))
        e.update({k: str(v) for k, v in env.items()})
        self.p = subprocess.Popen(
            ["taskset", "-c", cpus, binary / "krab-server"], env=e,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        for _ in range(200):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{self.port}/health", timeout=1).read()
                break
            except OSError:
                time.sleep(0.02)
        else:
            raise RuntimeError("krab-server did not come up")

    def url(self, to, src=None):
        q = f"to={FORMATS[to]}" + (f"&from={FORMATS[src]}" if src else "")
        return f"http://127.0.0.1:{self.port}/transform?{q}"

    def status(self, key):
        """VmRSS / VmHWM in bytes."""
        for line in Path(f"/proc/{self.p.pid}/status").read_text().splitlines():
            if line.startswith(key + ":"):
                return int(line.split()[1]) * 1024
        raise KeyError(key)

    def cpu_seconds(self):
        f = Path(f"/proc/{self.p.pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(f[11]) + int(f[12])) / os.sysconf("SC_CLK_TCK")

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.p.terminate()
        self.p.wait()


def oha(url, body, conns, cpus, seconds=None, n=None):
    cmd = ["taskset", "-c", cpus, "oha", "--no-tui", "--output-format", "json",
           "-m", "POST", "-D", str(body), "-c", str(conns)]
    cmd += ["-z", f"{seconds}s", "-w"] if seconds else ["-n", str(n)]
    cmd.append(url)
    r = json.loads(subprocess.run(cmd, check=True, capture_output=True, text=True).stdout)
    lat = r["latencyPercentiles"]
    return {
        "rps": r["summary"]["requestsPerSec"],
        "success": r["summary"]["successRate"],
        "statuses": r["statusCodeDistribution"],
        "total": sum(r["statusCodeDistribution"].values()),
        "p50_ms": (lat["p50"] or 0) * 1e3,
        "p99_ms": (lat["p99"] or 0) * 1e3,
        "max_ms": (r["summary"]["slowest"] or 0) * 1e3,
        "wall_s": r["summary"]["total"],
    }


def load(srv, url, body, conns, cpus, seconds):
    """Warm up, then one timed run; adds server CPU time per invoice."""
    oha(url, body, conns, cpus, seconds=1)
    c0 = srv.cpu_seconds()
    r = oha(url, body, conns, cpus, seconds=seconds)
    r["server_cpu_ms"] = (srv.cpu_seconds() - c0) / max(r["total"], 1) * 1e3
    return r


# ------------------------------------------------------------------ groups


def g_throughput(out):
    """Every route, 4 workers, by size."""
    scpu, lcpu = split_cpus(4)
    res = {}
    with Server(4, scpu) as srv:
        for lines in [3, 100, 1_000, 10_000]:
            conns, secs = (32, 6) if lines < 10_000 else (8, 10)
            for src in FORMATS:
                for dst in FORMATS:
                    key = f"{src}>{dst}@{lines}"
                    res[key] = load(srv, srv.url(dst, src), fixture(out, src, lines), conns, lcpu, secs)
                    print(key, round(res[key]["rps"]), flush=True)
    return res


def g_scaling(out):
    """UBL -> XRechnung by worker count (= pinned physical cores)."""
    res = {}
    for workers in range(1, len(physical_cpus()) + 1):
        scpu, lcpu = split_cpus(workers)
        with Server(workers, scpu) as srv:
            for lines in [3, 100] + ([1_000, 10_000] if workers == 1 else []):
                runs = [load(srv, srv.url("xrechnung"), fixture(out, "ubl", lines), 32, lcpu, 6) for _ in range(3)]
                runs.sort(key=lambda r: r["rps"])
                res[f"{workers}@{lines}"] = dict(runs[1], shared_cores=workers == len(physical_cpus()))
                print(workers, lines, round(runs[1]["rps"]), flush=True)
    return res


def g_cli(out):
    """One conversion end to end, UBL -> every format, by size."""
    res = {}
    cpu = str(physical_cpus()[2])
    j = out / "hyperfine.json"
    for lines in SIZES:
        for dst in FORMATS:
            runs = ["--runs", "5"] if lines >= 100_000 else ["--min-runs", "20"]
            subprocess.run(
                ["hyperfine", "-N", "--warmup", "2", *runs, "--export-json", j,
                 f"taskset -c {cpu} {GLIBC / 'krab-cli'} {fixture(out, 'ubl', lines)} {FORMATS[dst]} --out /dev/null"],
                check=True, stdout=subprocess.DEVNULL,
            )
            s = json.loads(j.read_text())["results"][0]["summary"]
            wall = s["time_wall_clock"]
            res[f"{dst}@{lines}"] = {
                "median_ms": wall["median"] * 1e3,
                "stddev_ms": wall["stddev"] * 1e3,
                "peak_rss": s["memory_peak_resident"]["median"],
                "instructions": s["instructions"]["median"],
            }
            print(dst, lines, round(wall["median"] * 1e3, 2), flush=True)
    j.unlink()
    return res


def one_request(binary, src, dst, body):
    """Peak RSS of a fresh server converting one invoice."""
    scpu, _ = split_cpus(4)
    with Server(4, scpu, binary) as srv:
        idle = srv.status("VmRSS")
        data = body.read_bytes()
        req = urllib.request.Request(srv.url(dst, src), data=data, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=600) as r:
                r.read()
                code = r.status
        except urllib.error.HTTPError as e:  # a refused route (422) still has a peak
            e.read()
            code = e.code
        return {"idle": idle, "peak": srv.status("VmHWM"), "size": len(data), "status": code}


def g_memory(out):
    res = {}
    for build, binary in [("glibc", GLIBC), ("musl", MUSL)]:
        for src in FORMATS:  # every route at 1k and 10k lines
            for dst in FORMATS:
                for lines in [1_000, 10_000]:
                    key = f"{build}:{src}>{dst}@{lines}"
                    res[key] = one_request(binary, src, dst, fixture(out, src, lines))
        for dst in FORMATS:  # UBL -> every format, every size
            for lines in SIZES:
                key = f"{build}:ubl>{dst}@{lines}"
                res.setdefault(key, one_request(binary, "ubl", dst, fixture(out, "ubl", lines)))
        print(build, "done", flush=True)
    return res


def g_concurrency(out):
    """k identical 10,000-line invoices sent at once to 4 workers."""
    scpu, lcpu = split_cpus(4)
    res = {}
    for k in [1, 2, 4, 8, 16]:
        with Server(4, scpu) as srv:
            idle = srv.status("VmRSS")
            r = oha(srv.url("xrechnung"), fixture(out, "ubl", LARGE), k, lcpu, n=k)
            res[k] = dict(r, idle=idle, peak=srv.status("VmHWM"))
            print(k, (res[k]["peak"] - idle) >> 20, "MB", flush=True)
    return res


def g_overload(out):
    """Eight 10,000-line invoices at once against a 300 MB budget."""
    scpu, lcpu = split_cpus(4)
    with Server(4, scpu, KRAB_MEM_BUDGET_BYTES=300 << 20) as srv:
        idle = srv.status("VmRSS")
        r = oha(srv.url("xrechnung"), fixture(out, "ubl", LARGE), 8, lcpu, n=8)
        return dict(r, idle=idle, peak=srv.status("VmHWM"))


def g_mixed(out):
    """Typical invoices alone, then while eight large ones keep 4 workers busy."""
    scpu, lcpu = split_cpus(4)
    small, large = fixture(out, "ubl", SMALL), fixture(out, "ubl", LARGE)
    with Server(4, scpu) as srv:
        url = srv.url("xrechnung")
        alone = load(srv, url, small, 8, lcpu, 6)
        with ThreadPoolExecutor(1) as ex:
            bg = ex.submit(oha, url, large, 8, lcpu, 16)
            time.sleep(3)
            busy = oha(url, small, 8, lcpu, seconds=8)
            bg.result()
    return {"alone": alone, "busy": busy}


GROUPS = {name[2:]: f for name, f in globals().items() if name.startswith("g_")}


def main():
    out = Path(sys.argv[1]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    for name in sys.argv[2:] or GROUPS:
        print(f"== {name}", flush=True)
        t = time.time()
        result = GROUPS[name](out)
        (out / f"{name}.json").write_text(json.dumps(result, indent=1))
        print(f"== {name}: {time.time() - t:.0f}s", flush=True)


if __name__ == "__main__":
    main()
