# observer-frog (Rust)

A port of the Python v0.1, built for distribution: one static binary, no runtime, no
extraction, about 8 MB resident.

## Why

The Python build works and is verified, but shipping it to strangers needs PyInstaller:
a 48 MB single-file executable that extracts itself to a temp directory on every launch, and
regularly trips Windows Defender as a false positive. This build is a 2.5 MB Mach-O/ELF/PE
binary that links only system libraries.

## Building

    cargo build --release          # target/release/observer-frog
    cargo test                     # 34 tests

Requires only a Rust toolchain. The web assets and the service catalog are embedded at compile
time, so the binary needs no files beside it.

## Verifying it matches the Python build

Run both against the same config and compare:

    python3 parity.py http://127.0.0.1:8800 http://127.0.0.1:8822

Volatile fields (cpu, memory, load, latency, network counters) are excluded by design.
Everything else must match exactly: endpoint key sets, service ids, every service's status
and identity fields, and the History KPIs. It reports PASS or lists each difference.

## What is identical

- All five probe kinds, and every `detail` string they produce
- The four invariants: down-confirmation (I1), absence sweep (I2), empty-payload guard (I3),
  silent first sighting (I4), plus ignored-excluded-from-uptime (I5) and serve-time flags (I6)
- Schema v2, byte-identical: a database written by either build opens in the other
- Every HTTP endpoint, response shape and value type
- The frontend, served byte-for-byte identical
- CLI output for status, config, catalog, add and metrics

## Measured, same config, both live

| | Python | Rust |
|---|---|---|
| Resident | 40.0 MB | 8.2 MB |
| Binary / install | 48 MB (PyInstaller) | 2.5 MB |
| Startup | 40 ms | under 1 ms |
| Tests | 161 | 34 |

The test gap is honest: the Python suite has 161 assertions accumulated over the evening,
including every frontend and server edge case. The Rust suite covers the state machine
invariants, the probe kinds, schema migration and the notification rules. Parity is enforced
by `parity.py` against the running Python build rather than by test count.

## Platform notes found during the port

- On macOS a plain process refresh returns an EMPTY cmdline for every process; `cmd` must be
  requested explicitly via `ProcessRefreshKind::with_cmd(Always)`. Without it every `process`
  probe reads "not running".
- `statvfs` over-reports disk usage on APFS (129 GB instead of 13 GB) because the read-only
  system snapshot shares blocks with the data volume. psutil corrects it with a native call;
  this build uses `df -k`, which reports the same corrected figure.
- rustls 0.23 panics at first use unless a crypto provider is installed explicitly.
