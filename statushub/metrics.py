"""Host metrics via psutil, plus the platform-specific honest extras.

Design rule: a metric that cannot be read on this platform is reported as `None` and the
renderer shows "n/a". It is never guessed and never rendered as a fault (this is the same
rule as probes: `unavailable`, not `down`).

psutil gives portable CPU/memory/disk/network/uptime. macOS additionally gets memory
pressure detail and thermal state; Linux gets a real die temperature; Windows gets neither.
"""

from __future__ import annotations

import os
import platform
import sys
import socket
import subprocess
import time

KIND_NAMES = {"darwin": "mac", "win32": "windows"}


def kind() -> str:
    return KIND_NAMES.get(sys.platform, "linux")


def _gb(n: float) -> float:
    return n / (1024 ** 3)


def _mb(n: float) -> float:
    return n / (1024 ** 2)


def _pct(part: float, whole: float) -> float | None:
    return round(100.0 * part / whole, 1) if whole else None


def cpu_pct(sample_s: float = 0.25) -> float | None:
    try:
        import psutil
        return round(psutil.cpu_percent(interval=sample_s), 1)
    except Exception:
        return None


def memory() -> dict:
    try:
        import psutil
        vm = psutil.virtual_memory()
        swap = psutil.swap_memory()
    except Exception:
        return {"mem_used": None, "mem_total": None, "mem_pct": None,
                "swap_used": None, "mem_cached": None, "mem_compressed": None}

    out = {
        "mem_used": round(_gb(vm.used), 1),
        "mem_total": round(_gb(vm.total), 2),
        "mem_pct": vm.percent,
        "swap_used": f"{_mb(swap.used):.0f} MB" if swap.total else None,
        "mem_cached": f'{_gb(getattr(vm, "cached", 0)):.1f} GB cached' if getattr(vm, "cached", 0) else None,
        "mem_compressed": None,
    }
    # The renderer expects `mem_used` as a display string ("9.6/16 GB"); the raw numbers ride
    # alongside as mem_used_gb/mem_total_gb so nothing has to parse the string back.
    if out["mem_total"]:
        out["mem_used_gb"] = out["mem_used"]
        out["mem_used"] = f"{out['mem_used']:.1f}/{out['mem_total']:.0f} GB"
    else:
        out["mem_used"] = ""
    out.pop("mem_used_str", None)
    return out


def macos_memory_detail() -> dict:
    """vm_stat accounting: used = App + Wired + Compressed, the definition Activity Monitor uses.

    psutil's `used` on macOS includes cacheable pages and reads higher than the OS UI, so when
    vm_stat is available we prefer it and say so.
    """
    if sys.platform != "darwin":
        return {}
    try:
        out = subprocess.run(["vm_stat"], capture_output=True, text=True, timeout=5).stdout
        import re
        page_m = re.search(r"page size of (\d+) bytes", out)
        page = int(page_m.group(1)) if page_m else 16384

        def pages(label: str) -> int:
            m = re.search(label + r":\s+(\d+)", out)
            return int(m.group(1)) if m else 0

        used = (pages("Anonymous pages") + pages("Pages wired down")
                + pages("Pages occupied by compressor")) * page
        cached = pages("File-backed pages") * page
        compressed = pages("Pages occupied by compressor") * page
        total = 0
        try:
            total = int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True,
                                                timeout=5).strip())
        except Exception:
            pass
        if not total:
            return {}
        return {
            "mem_used": f"{_gb(used):.1f}/{_gb(total):.0f} GB",
            "mem_used_gb": round(_gb(used), 1),
            "mem_total": round(_gb(total), 2),
            "mem_pct": _pct(used, total),
            "mem_cached": f"{_gb(cached):.1f} GB cached",
            "mem_compressed": f"{_gb(compressed):.1f} GB",
            "_source": "vm_stat",
        }
    except Exception:
        return {}


def thermal() -> dict:
    """macOS thermal pressure (no root needed). Linux: die temperature when readable."""
    if sys.platform == "darwin":
        try:
            out = subprocess.run(["pmset", "-g", "therm"], capture_output=True, text=True,
                                 timeout=5).stdout
            import re
            m = re.search(r"CPU_Speed_Limit\s*=\s*(\d+)", out)
            return {"thermal": f"throttled {m.group(1)}%" if m else "Nominal", "temp_c": None}
        except Exception:
            return {"thermal": None, "temp_c": None}
    try:
        import psutil
        getter = getattr(psutil, "sensors_temperatures", None)
        temps = getter() if getter else {}
        for entries in (temps or {}).values():
            for e in entries:
                if e.current is not None:
                    return {"thermal": None, "temp_c": round(float(e.current), 1)}
    except Exception:
        pass
    return {"thermal": None, "temp_c": None}


def disk() -> dict:
    try:
        import psutil
        du = psutil.disk_usage(os.path.abspath(os.sep))
        return {"disk_used": f"{_gb(du.used):.1f}/{_gb(du.total):.0f} GB",
                "disk_pct": du.percent}
    except Exception:
        return {"disk_used": "", "disk_pct": None}


def uptime_hours() -> float | None:
    try:
        import psutil
        return round((time.time() - psutil.boot_time()) / 3600.0, 1)
    except Exception:
        return None


def load_avg() -> list[float] | None:
    try:
        return [round(x, 2) for x in os.getloadavg()]
    except (OSError, AttributeError):
        return None


def network() -> dict:
    try:
        import psutil
        io = psutil.net_io_counters()
        return {"net_sent_mb": round(_mb(io.bytes_sent), 0),
                "net_recv_mb": round(_mb(io.bytes_recv), 0)}
    except Exception:
        return {}


def device_card(node_id: str = "local", node_name: str | None = None) -> dict:
    """The `devices[]` entry for this machine, in the shape the UI already renders."""
    k = kind()
    # Shape: "<chip or CPU model> - <OS>", e.g. "Example CPU - Linux".
    detail_bits = [platform.processor() or platform.machine(), platform.system()]
    if platform.system() == "Darwin":
        detail_bits[0] = _mac_chip() or detail_bits[0]
        detail_bits[1] = "macOS"
    elif platform.system() == "Linux":
        detail_bits[1] = _linux_pretty_name() or platform.system()
        detail_bits[0] = _linux_cpu_model() or platform.machine()

    card = {
        "id": f"dev_{node_id}",
        "name": node_name or socket.gethostname(),
        "kind": k,
        "status": "up",
        "detail": " - ".join(b for b in detail_bits if b),
        "cpu_pct": cpu_pct(),
        "uptime_h": uptime_hours(),
    }
    card.update(memory())
    mac_detail = macos_memory_detail()
    if mac_detail:
        card.update(mac_detail)
    card.update(thermal())
    card.update(disk())
    card.update(network())
    card["load_avg"] = load_avg()
    return card


def _mac_chip() -> str | None:
    try:
        return subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"],
                                       text=True, timeout=5).strip()
    except Exception:
        return None


def _linux_cpu_model() -> str | None:
    try:
        from pathlib import Path
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith(("model name", "hardware", "model")):
                return line.split(":", 1)[1].strip()
    except Exception:
        return None
    return None


def _linux_pretty_name() -> str | None:
    try:
        from pathlib import Path
        for line in Path("/etc/os-release").read_text().splitlines():
            if line.startswith("PRETTY_NAME="):
                return line.split("=", 1)[1].strip().strip('"')
    except Exception:
        return None
    return None
