# Zyphra fork: cooperative blocking reads

## Problem

WASIX `dlopen` is a stop-the-world operation: every thread has its own function
table, so the loading thread raises a flag, sends `Sigwakeup` to all threads, and
waits until each one has entered `do_pending_link_operations` (see the module docs
in `lib/wasix/src/state/linker/mod.rs`). Threads blocked in a syscall only
cooperate if that syscall's wait loop services signals and link operations.

`fd_read` waits through `__asyncify_light`, which only calls `block_on`. A thread
blocked reading stdin or a pipe therefore stalls every other thread's `dlopen`
until data arrives. In PyWasmer, the main thread always waits on stdin for the
next RPC message, so any native extension imported at runtime (Pillow's WebP
codec, `importlib.import_module`, ...) hangs the kernel. `poll_oneoff` is not
affected: it waits through `AsyncifyPoller`, which does service both.

Upstream: issues wasmerio/wasmer#6472 (this hang) and #6426 (signals ignored by
blocked pipe reads). Upstream PR #6485 switches `fd_read` to `__asyncify`, but
keeps raw guest-memory pointers across the wait to avoid a copy; it was moved
back to draft because guest memory can move during that wait.

## Fix

Before a blocking read from a source that can block indefinitely, wait until the
source is readable through `__asyncify_cooperative`, which blocks on
`AsyncifyPoller` like `poll_oneoff` does. Then run the existing read unchanged.

- `syscalls/mod.rs`: `__asyncify_cooperative(ctx, work)`, a thin `block_on`
  over `AsyncifyPoller` (link operations, signal handlers, forced exit).
- `syscalls/wasi/fd_read.rs`: `read_readiness(&fd_entry)` returns a future over
  `VirtualFile::poll_read_ready` for stdio files, `PipeRx`, and `DuplexPipe`;
  `fd_read_internal` awaits it first unless the descriptor is non-blocking.
- `virtual-fs/src/host_fs.rs`: host `Stdin::poll_read_ready` stages 64 KiB
  instead of 8 KiB. Readiness reads into that staging buffer and the next read
  drains it, so its size bounds each stdin read; tokio's blocking stdin performs
  one thread hand-off per staged chunk.

The wait never touches guest memory, so it holds no raw pointers across the
wait, which avoids the objection that stalled #6485.

## Behavior and cost

- Data already available: one extra `poll_read_ready` call, then the same read.
- Regular files, sockets, event fds, and non-blocking descriptors are unchanged.
- Stdin data passes through the 64 KiB staging buffer, one extra copy. With the
  original 8 KiB staging, bulk stdin was 6.5x slower (256 MiB in 1.1 s instead
  of 0.17 s); 64 KiB is within noise of the unpatched binary.
- Signals arriving during the wait run their handlers and the wait continues,
  as in `poll_oneoff`; there is no new `EINTR`. `Sigint`/`Sigquit`/`Sigkill`/
  `Sigabrt` still terminate.
- Not covered: blocking socket reads, and data consumed by another thread
  between readiness and read, which falls back to the old non-cooperative wait.

## Replaying

The patch touches only the three files above. Replay it after the volume patches
(see [ZYPHRA_FORK.md](./ZYPHRA_FORK.md)); if upstream's `fd_read` has changed,
keep the shape: wait for readiness cooperatively, then read as upstream does.
Drop the patch once upstream ships an equivalent fix (#6485 or successor);
verify with the reproduction below before dropping it.

## Validation

PR #6485 adds a guest fixture, `fd_tests/stdin-dlopen-race`, which needs
`wasixcc`; it is not ported here. Validate with Python instead: one thread imports
a native extension while the main thread blocks in `sys.stdin.buffer.readline()`.

```python
import sys, threading, time
t0 = time.monotonic()
def f():
    time.sleep(2)
    import PIL._webp
    print(f"{time.monotonic()-t0:5.1f}s import done", flush=True)
threading.Thread(target=f, daemon=True).start()
sys.stdin.buffer.readline()
```

Feed it `(sleep 15; echo) |`. Without the fix the import finishes at ~15 s, when
stdin delivers; with it, at ~2 s. Then rerun the PyWasmer kernel benchmarks and
stdin/pipe/file throughput against the previous binary.

## Results (`v7.5.0` replay, aarch64 Linux, LLVM backend)

| | 7.5.0 without fix | 7.5.0 with fix |
|---|---|---|
| Reproduction: import while main blocks on stdin | 11.4 s (waits for stdin) | 2.0 s |
| 256 MiB stdin, 1 MiB reads (5 runs) | 0.16-0.22 s | 0.18-0.20 s |
| 256 MiB in-guest pipe / 256 MiB regular file | 0.02 s / 0.05 s | 0.02 s / 0.05 s |
| PyWasmer RPC: cell round trip, 200 sequential tool calls | 0.3-0.5 ms, 0.3 ms each | 0.4-0.5 ms, 0.3 ms each |
| PyWasmer RPCM: cell round trip, 200 sequential tool calls | 0.6-0.8 ms, 0.4 ms each | 0.7-0.8 ms, 0.4 ms each |
| PyWasmer idle CPU over 10 s | 0.00 s | 0.00 s |
| Two kernels importing native modules at runtime | times out | 4-12 ms |

PyWasmer `tests/test_rpc_behavior.py` (commit `20c8548`) passes 158/160 against
the fixed binary; the two `package_preload` tests fail identically on the 7.2.1
fork binary without starting a sandbox.
