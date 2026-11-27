# Dependency diagnostics

`victory --diagnostic-jsonl FILE` writes structured events to a new file outside
the source tree. The parent directory must exist. Ordinary command output and
the `VICTORY_LOG` console filter continue to work. The flag is available in
ordinary builds and builds with the `diagnostics` feature. File creation
and write failures appear on stderr and leave the command result intact.

The emitter records image read failures in `inspect`. On Windows, it also
records image read failures in `run`, failures creating the directory inside
the image loader, and failures from its `LoadLibraryExW` call. A missing synthetic
file exercises the CLI without running guest code.

```powershell
$capture = Join-Path $env:TEMP ("victory-" + [guid]::NewGuid() + ".jsonl")
$absent = Join-Path $env:TEMP ("victory-" + [guid]::NewGuid() + ".exe")
cargo run --locked -p victory -- --diagnostic-jsonl $capture inspect $absent
cargo xtask diagnostics $capture
```

The inspect command is expected to return an error. The report shows the first
observed operation failure, distinct incidents and occurrence counts, and
whether the capture contains a complete session footer. A completed
session can still have a failed command. Test counts are based on explicit test
events in the input. CLI captures have zero test counts.

## Event format

Each complete line is a JSON event for schema version `1.0`. The timestamp is
UTC milliseconds since the Unix epoch. The sequence orders events written
within the session. Each observed operation failure starts a correlation group.
The current instrumentation reports one event per operation failure. A
correlation ID alone does not establish a causal link across operations.

The diagnostic definition describes the returned code. The root cause stays
`undetermined` unless evidence establishes it. The loader's Windows binding
version is read from the checked-in lockfile and identified as a locked version.
For builds that provide it, `VICTORY_BUILD_COMMIT` is read at compile time and
included only if it contains 40 or 64 hexadecimal digits.

Only fields allowlisted by the emitter are written. The JSONL excludes path
names, native messages that can contain user or guest data, and guest data from
the existing crash and capture facilities. The Rust error still carries its
original source for ordinary error handling. Limits are fixed at 4 MiB per
session for this implementation. A normal close reports any events omitted to
stay within that limit. A failed write or process termination can leave an
incomplete capture.
