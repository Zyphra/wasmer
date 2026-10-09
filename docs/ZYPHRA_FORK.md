# Zyphra Wasmer fork

## Purpose and base

This fork adds mounted-volume policies for PyWasmer workers, plus a blocking-read
fix documented separately in [ZYPHRA_FD_READ.md](./ZYPHRA_FD_READ.md). The
maintained branch is `zyphra`; upstream is `https://github.com/wasmerio/wasmer.git`.
The current base is upstream `v7.5.0`, commit
`82ff099e082da586b38af79986abbb8b677f8baf` (previously `v7.2.1`, commit
`c14032594b893b40e9b71456d504cf55c141c8f6`; that branch is kept as
`backup/zyphra-7.2.1`).

The CLI accepts one JSON mount declaration, for example:

```sh
--mount '{"source":"/host/cache","target":"/cache","max_growth_bytes":104857600}'
--mount '{"source":"/host/input","target":"/input","read_only":true}'
```

The original `--volume HOST:GUEST` remains supported. Limits are optional byte
counts; read-only mounts cannot also specify a growth limit. JSON preserves path
characters without custom delimiter escaping and rejects unknown/duplicate fields.
Guest destinations are normalized before duplicate and policy validation.
The corresponding `MappedDirectory` fields default to writable and unlimited.

## Accounting choices

- Each limited mount has one shared atomic counter. This is approximate logical
  net growth, not physical disk usage or a worker-wide cap across all mounts.
- Existing contents are an uncounted baseline: no initial scan, provenance map,
  or directory-sized accounting state. Shrinking and deletion restore allowance;
  deleting pre-existing contents can therefore reduce the counter too.
- State is constant per mount and open handle. Writes use cached size and
  position; reservation failures return `ENOSPC` without terminating the worker.
- WASIX's existing inode-level handle is shared by guest descriptors. Namespace
  removal refunds contents through the filesystem, then `InodeGuard::on_unlink`
  notifies the live `VirtualFile`. Its refunded prefix must not be refunded again
  by truncation or close. Growth after unlink is charged until shrink or last close.
- Deletion and replacement finish pending writes before consulting backing
  metadata. They coordinate with shared-handle installation through the existing
  fd-map lock; they do not introduce a second open-file registry.
- Before `path_open` replaces an existing regular-file handle, it flushes pending
  writes on that handle. Quota wrappers initialize per-handle size from backing
  metadata, so replacement must not race writes already accepted by the old
  handle. This adds no accounting state and is a flush, not a durability `fsync`.
- Successful rename replaces the cached destination inode while old descriptors
  retain the displaced inode. Compare inode objects, not path-derived inode
  numbers, when identifying a same-file rename. Failed operations retain entries.
- Within a host mount, native rename preserves open-file identity and avoids
  copying contents between directories. It preserves the file's modification time.

The read-only wrapper rejects path and handle mutations with `EROFS`. Preserve
both `EROFS` and `ENOSPC` through Rust I/O, virtual-fs, WASI, and guest Python.
WASIX's optimistic duplex open must retry with requested rights on `EROFS`.

## Boundaries and deferred work

This is an approximate limiter, not a strict storage-security boundary. External
mutations, independent handles/aliases, and deletion of baseline contents can
skew accounting. Directory entries and other guest filesystems are not charged.
Cross-volume rename retains the existing copy-and-delete behavior; coordination
with live source/destination handles is deferred in `ops::move_across_filesystems`.

Read-only hard-link handling and consistent `EROFS` from timestamp operations
remain deferred. WASIX chmod is a no-op. Do not interpret the wrapper as fixing
every syscall's read-only semantics.

No 512 MiB default or MAIA wiring is added here. PyWasmer configuration and Python
RPC regression tests live in the separate PyWasmer checkout. Official Wasmer
binaries do not implement these flags; distribution must pin a fork build.

## Updating upstream

Fetch upstream tags, back up the maintained branch, then replay only our patches
onto the new tag. Cherry-pick rather than `git rebase --onto`: the submodule
checkout is shallow, so old and new tags share no history and rebase cannot find
the patches' base. Cherry-pick only needs each patch and its parent.

```sh
git fetch upstream --tags
git branch backup/zyphra-<previous-version> zyphra
git switch -c zyphra-<new-version> <new-upstream-tag>
git cherry-pick <previous-upstream-base>..zyphra   # or list the fork commits
git submodule update --init --depth=1 lib/napi wasmer-test-files
```

Update the base recorded above after each replay. Upstream also moves the Rust
toolchain (`rust-toolchain.toml`) and occasionally LLVM (`inkwell`'s `llvmNN-M`
feature); MAIA's native recipe in `.dev/wasmer/` pins both and must follow.
Preserve behavior rather than restoring old code if upstream changes its APIs:

- `virtual-fs/{quota_fs,read_only_fs,lib,host_fs}.rs`: accounting, notification
  contract, native rename, and precise errors.
- `wasix/src/fs/` and `syscalls/wasi/path_{rename,unlink_file}.rs`: shared handle
  ownership, lock order, namespace identity, and exactly-once removal notification.
- `runners/wasi_common.rs`, CLI parsing, and `path_open2.rs`: mount policies,
  normalization, constructor defaults, and read-only open fallback.

Before replaying, diff the `virtual-fs` `FileSystem` and `VirtualFile` traits
between the two tags. A new method with a default implementation compiles but
bypasses the quota and read-only wrappers until they override it.

The `v7.2.1` to `v7.5.0` replay conflicted in three places:

- `wasi_common.rs`: upstream replaced `cfg_if!` with `cfg_select!` and added
  `os_env_vars` beside the free `normalized_mount_path`, which our patch moves
  into `MappedDirectory::normalize_guest_path`. Keep `os_env_vars`, drop the free
  function, and port the policy wrappers into the `cfg_select!` arm.
- `path_unlink_file.rs`: upstream returns `NOENT` (or removes an ephemeral
  symlink) when the parent has no entry, and routes symlink removal through
  `WasiFs::remove_symlink_file`. Keep our order (flush, remove backing file,
  remove the entry, `on_unlink`) and take both upstream behaviors, checking the
  missing entry first as upstream does.
- `cli/.../run/wasi.rs`: both sides added a test module at the end of the file;
  keep both.

Use the checked-in Rust toolchain. Run formatting, virtual-fs tests, CLI volume
tests, and a CLI build. The initial validation used:

```sh
cargo fmt --all --check
cargo test --locked -p virtual-fs
cargo test --locked -p wasmer-cli --no-default-features --features cranelift,wat volume
cargo build --locked -p wasmer-cli --no-default-features --features cranelift,wat
```

Run PyWasmer RPC tests against that newly built binary: quota exhaustion and
worker survival, deletion/shrink refunds, repeated replacement with old handles
open, moves between directories, truncation through another descriptor, and
unlinked growth released on last close. The truncation test must write to one
descriptor and immediately open a second writable descriptor without an explicit
flush; this verifies that handle replacement cannot initialize quota state from
stale backing metadata. Also check failed rename, directories, symlinks, and
read-only errors. Initial results: 130 filesystem tests passed
(two existing ignores), four CLI volume tests, and 12 quota RPC cases passed.

The `v7.5.0` replay was validated the same way, plus
`cargo test --locked -p wasmer-wasix --features sys,host-fs --lib` and the
`lint-packages` clippy flags on the touched crates: 136 virtual-fs tests (two
existing ignores), two CLI volume tests (the JSON `--mount` rework left two),
230 wasix unit tests (two existing ignores), and 37 volume/quota/read-only cases
of PyWasmer `tests/test_rpc_behavior.py` on both RPC and RPCM. A local LLVM build
needs `libffi-dev` and `libxml2-dev`, as installed by MAIA's Dockerfile.

After review, update the published branch with `git push --force-with-lease origin
zyphra` and pin consumers to the new tested commit/build.
