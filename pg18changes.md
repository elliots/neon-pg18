# Adding PostgreSQL 18 support

What had to change to run Neon on PostgreSQL 18, and why. Written while doing the
port, so it records the reasoning and the evidence, not just the diff.

Two forks are involved:

| repo | branch | role |
|---|---|---|
| `elliots/neon-postgres` | `REL_18_STABLE_neon` | the vendored PostgreSQL, plus one Neon-specific commit |
| `elliots/neon-pg18` | `main` | this repository |

`vendor/postgres-v18` is pinned to PostgreSQL 18.2.

---

## 1. Why this is not just "add 18 to the list"

Most of a Neon major-version bump is enumeration: teach `PgMajorVersion`, the
Makefile, the Dockerfiles and CI about a new number. That part is mechanical.

The rest is not, because PostgreSQL 18 changed three things Neon depends on
deeply:

1. **All buffer reads go through the AIO subsystem.** There is no longer a
   synchronous `smgrreadv()` path for the buffer manager to fall back to. Neon
   fetches pages over the network, so it has no file descriptor to hand to an
   asynchronous read.
2. **The storage manager is registered, not hooked.** `smgr_hook` is gone,
   replaced by `smgrregister()` plus an ownership callback.
3. **initdb enables data checksums by default.** Neon serves pages that never
   passed through a buffer manager, and those carry no checksum.

Everything else below is comparatively routine.

---

## 2. On-disk formats

Two structures Neon parses changed. Both were found by compiling a
`sizeof`/`offsetof` probe against the v17 and v18 headers rather than by reading
release notes, and both are silent corruption if missed — no compiler error, just
wrong bytes.

### 2.1 `xl_xact_stats_item` grew from 12 to 16 bytes

PostgreSQL 18 replaced the 32-bit `Oid objoid` field with a 64-bit object id
split across `objid_lo`/`objid_hi`.

```
          v17   v18
sizeof     12    16
```

`XlXactParsedRecord::decode()` skipped these records with a hardcoded `12`. On a
v18 commit record carrying dropped-statistics entries, the parser would advance
by the wrong number of bytes and desynchronise for the remainder of the record —
so `XACT_XINFO_HAS_INVALS`, `HAS_TWOPHASE` and `HAS_ORIGIN` would all be read
from the wrong offsets.

The size is now a per-version constant (`SIZEOF_XL_XACT_STATS_ITEM` in
`pg_constants_v*.rs`) and `decode()` takes a `pg_version`.

### 2.2 `ControlFileData.crc` moved from offset 288 to 292

v18 inserted `bool default_char_signedness` ahead of `mock_authentication_nonce`:

```
                              v14..v17   v18
data_checksum_version              252   252
default_char_signedness              -   256
mock_authentication_nonce          256   257
crc                                288   292
sizeof(ControlFileData)            296   296
```

`sizeof` is unchanged, so nothing obvious breaks — the CRC is simply read from
the wrong place, and every v18 control file is rejected as corrupt:

```
invalid CRC in control file: expected 816A4611, was 00000099
```

The awkward part is that callers cannot know the version up front:
`import_pgdata` *derives* the Postgres version from `catalog_version_no` inside
the very file it is trying to validate. The fix is `decode_control_file()`, which
reads `pg_control_version` — at a fixed offset in every supported version — and
then validates the CRC at the offset that version uses. The per-version offsets
come from `offset_of!` on each vendored header, so they stay correct if a future
minor release shifts them.

Every caller has to go through it, and one did not: `pagectl` decoded control
files with `ControlFileData::decode()`, which validates the CRC at the v14 offset
unconditionally. `pagectl print-layer-file` therefore rejected every v18 control
file as corrupt — a diagnostic tool reporting corruption in a healthy cluster,
which is worse than it merely not working. It now calls `decode_control_file()`
like everything else.

### 2.3 Everything else is unchanged

Verified byte-identical between v17 and v18: `CheckPoint`, `XLogRecord`,
`XLogPageHeaderData`, `XLogLongPageHeaderData`, all `xl_heap_*`, `xl_smgr_*`,
`xl_dbase_*`, `xl_multixact_*`, `xl_relmap_update`, `xl_replorigin_*`,
`xl_end_of_recovery`, `xl_parameter_change`, `xl_running_xacts`,
`SharedInvalidationMessage`, and the record/block header sizes.

That is why the v18 WAL decoders are shared with v17 rather than duplicated.
`XLOG_PAGE_MAGIC` went `0xD116` → `0xD118` and `PG_CONTROL_VERSION` `1700` →
`1800`, both picked up automatically through bindgen.

---

## 3. Storage manager and asynchronous I/O

### 3.1 The problem

In v18 the buffer manager reads **only** through `smgrstartreadv()`. There is no
`smgr_readv()` fallback. `md` satisfies this by starting a real `preadv()` against
a file descriptor.

Neon has no file descriptor. And an out-of-tree storage manager cannot complete an
I/O by other means:

- `pgaio_io_start_readv()` takes an `fd`.
- `pgaio_io_stage()` with `PGAIO_HF_SYNCHRONOUS` routes into
  `pgaio_io_perform_synchronously()`, which issues `pg_preadv()` on
  `ioh->op_data.read.fd`.
- Driving the handle state machine directly is impossible because
  `pgaio_io_update_state()` is `static` in `aio.c`.

### 3.2 The fix, in the PostgreSQL fork

`pgaio_io_complete_synchronously(ioh, op, result)` — one commit on
`elliots/neon-postgres@REL_18_STABLE_neon`. The caller sets the I/O's target,
registers its callbacks and fills the buffers itself; this then runs the handle
through the same staging and completion sequence `pgaio_io_stage()` would, minus
the syscall. Completion callbacks above smgr — `md_readv_complete()` and the
shared/local buffer callbacks — behave exactly as they do for a file-backed read.

It is written as a general facility for any storage manager that produces page
data without a file descriptor, not as a Neon special case, so it is plausibly
upstreamable.

`neon_startreadv()` and the WAL redo process's `inmem_startreadv()` both use it:
fetch the pages synchronously, then hand the completed I/O back.

`PGAIO_HCB_MD_READV` is registered alongside, exactly as `md` does. Despite the
name it is not md-specific — it converts the byte count into a block count and
reports short reads from the smgr target data.

### 3.3 Registration replaced hooks

| v17 | v18 |
|---|---|
| `smgr_hook` returns an `f_smgr*` | `smgrregister()` returns a `SmgrId` |
| — | `smgr_owns(rlocator, backend, relpersistence)` decides ownership |
| `f_smgr.smgr_start_unlogged_build` etc. | standalone `start_unlogged_build_hook` etc. |
| `f_smgr.smgr_read_slru_segment` | standalone `read_slru_segment_hook` |
| — | new members: `smgr_name`, `smgr_maxcombine`, `smgr_startreadv`, `smgr_fd` |

`smgropen()` consults registered smgrs in reverse registration order, so Neon's
`smgr_owns()` is asked before md's and declines temporary relations
(`backend != INVALID_PROC_NUMBER`) — reproducing what `smgr_neon()` did before.

`smgr_fd()` raises an error: it exists so the AIO subsystem can re-issue an I/O
in another process, which cannot apply to pages fetched over the network.

### 3.4 `smgrinit()` is now per-backend

A subtle one. In v17, `smgr_init_hook` replaced the whole of `smgrinit()`. In v18,
`smgrinit()` runs per backend from `InitPostgres()` and calls each registered
smgr's `smgr_init`.

So the per-backend work that used to live in the hook — `RegisterXactCallback()`,
`neon_init()`, `communicator_init()` — moved into the `smgr_init` callback, and
`smgr_init_neon()` now only performs the one-time registration from `_PG_init()`.
Registering in the postmaster and inheriting across `fork()` is correct; doing the
per-backend setup there would not have been.

### 3.5 The SLRU download hook changed shape

```c
/* v17 */ int  (*)(SMgrRelation, const char *path, int segno, void *buffer)
/* v18 */ bool (*)(const char *path, int64 segno)
```

The hook now owns writing the segment file and reports only whether the segment
was found; previously `slru.c` allocated the buffer and did the writing.

### 3.6 `io_method = sync`

v18 defaults to `io_method = worker`, which hands I/Os to worker processes. Neon
completes its reads inline, so the workers have nothing to do. Both the compute
and the WAL redo process are configured for `sync`.

This matches how Neon runs v18 in production: *"On Neon, PostgreSQL 18 currently
runs with `io_method = 'sync'` for stability during the preview period. Full async
I/O support is coming soon."*

For the WAL redo process it is not merely an optimisation — there is no postmaster
to run I/O workers, and its seccomp filter permits only a handful of syscalls.

Setting it for the compute takes one more step than it looks. `compute/manifest.yaml`
carries the per-version settings the control plane applies, and its `per_version`
map had entries for 14–17 only; its schema restricted the keys to `^1[4-7]$`, so
adding an `18:` block without widening the pattern would have been rejected rather
than applied. Both are updated, with `io_method: sync`, `io_combine_limit: 1` (as
v17 has, for the same reason: read-ahead over the network costs more than it
saves), and `recovery_prefetch: off` for replicas.

Nothing in this repository reads that file — the control plane does — so nothing
here would have caught its absence. The tests all configure computes directly.

---

## 4. The WAL redo process

`neon_walredo` builds a cut-down equivalent of `InitPostgres()`, initialising only
the subsystems it needs. v18 reorganised that initialisation in two ways.

### 4.1 Renames

| v17 | v18 |
|---|---|
| `InitBufferPool()` | `BufferManagerShmemInit()` |
| `InitLocks()` | `LockManagerShmemInit()` |
| `InitPredicateLocks()` | `PredicateLockShmemInit()` |
| `CreateSharedProcArray()` | `ProcArrayShmemInit()` |
| `CreateSharedBackendStatus()` | `BackendStatusShmemInit()` |
| `CreateSharedInvalidationState()` | `SharedInvalShmemInit()` |

Also newly required: `InitPostmasterChildSlots()` (v18 keeps postmaster child
slots in a pool that several shmem sizing routines consult),
`InitializeFastPathLocks()`, and `AioShmemInit()`.

### 4.2 The renames hid a split — and caused a SIGSEGV

This was the hardest bug in the port. The redo process died with `signal: 11`
as soon as it replayed a record that took a relation extension lock:

```
hash_search (hashp=0x0)          dynahash.c:963
LockAcquireExtended ()           lock.c:893
LockRelationForExtension ()      lmgr.c:432
ExtendBufferedRelTo ()           bufmgr.c:962
vm_extend ()                     visibilitymap.c:622
redo_neon_heap_multi_insert ()   neon_rmgr.c:783
```

v18 did not merely rename these routines, it **split the per-backend half out of
them**. `InitLocks()` used to create both the shared lock hashes *and* this
backend's LOCALLOCK hash. `LockManagerShmemInit()` only does the former, so
`LockAcquire()` dereferenced a NULL hashtable.

Because this process never reaches `BaseInit()`, it now explicitly calls the three
per-backend routines v18 requires:

| | why |
|---|---|
| `pgaio_init_backend()` | sets `pgaio_my_backend`, which every AIO entry point dereferences |
| `InitBufferManagerAccess()` | private refcount hash, and `MaxProportionalPins` which the v18 read path consults |
| `InitLockManagerAccess()` | the LOCALLOCK hash above |

### 4.3 `smgrinit()` per record exhausts the `on_proc_exit` slots

`ApplyRecord()` calls `smgrinit()` once per applied record, to reset the
in-memory smgr. On v14–v17 that is free. v18 ends `smgrinit()` with:

```c
	/* register the shutdown proc */
	on_proc_exit(smgrshutdown, 0);
```

`on_proc_exit` has a hard cap of `MAX_ON_EXITS` (20) and no de-duplication, so on
v18 every applied record burned a slot and the process died on its 20th record:

```
FATAL:  out of on_proc_exit slots
```

The pageserver respawned walredo, which died on *its* 20th record, forever. In one
run the log held **18,700 distinct walredo pids, every one with this FATAL**. The
compute never got its page back, so it reconnected and retried GetPage every
~17 ms indefinitely:

```
LOG: [NEON_SMGR] [shard 0] could not get response from pageserver: server closed the connection
LOG: [NEON_SMGR] [shard 0] No response from reading prefetch entry 3480: 1663/5/16399.0 block 775
```

Any test doing enough bulk writing hung rather than failed. `pgbench -i` was the
usual trigger: `test_branching_with_pgbench` (both variants) and
`test_branching_unnormalized_start_lsn` each sat at the 900 s pytest timeout on
v18 while passing on v17.

v18 now resets the one smgr this process has directly, which is all the call was
ever for here:

```c
#if PG_MAJORVERSION_NUM >= 18
	smgr_init_inmem();
#else
	smgrinit();					/* reset inmem smgr state */
#endif
```

All three tests pass afterwards, the slowest in 18 s rather than timing out.

Worth noting for anyone reading a similar hang: the failure is **load-dependent,
not startup-dependent**. A walredo process starts fine on v18 and survives 19
records, so anything short of ~20 records looks healthy.

### 4.4 Debugging note: seccomp hides the error

The redo process closes its file descriptors and enters seccomp before doing any
work. `abort()` and the SIGSEGV handler are themselves killed by `SIGSYS`, so
**nothing reaches the pageserver's stderr capture** — the failure looks like a
silent `read walredo stdout: early eof`.

The signal is visible in the pageserver's `exit_status`. To get a backtrace, run
the test with `ulimit -c unlimited` and point gdb at the core the redo process
leaves in the pageserver directory:

```sh
gdb -q -batch -ex "bt 25" pg_install/v18/bin/postgres core
```

---

## 4b. Index builds and buffer eviction

`smgr_start_unlogged_build()` sends an index build's writes to local storage, and
the access method only WAL-logs the finished index -- the `log_newpage_range()`
call between `smgr_finish_unlogged_build_phase_1()` and
`smgr_end_unlogged_build()` in `spginsert.c`, `gininsert.c` and `gistbuild.c`.
Until that runs, the index pages sit dirty in *shared* buffers carrying a
placeholder LSN: `InvalidXLogRecPtr` for SP-GiST and GIN, and `GistBuildLSN`
(the literal 1, from `gist.h`) for GiST.

Any backend that needs a buffer can evict those pages, not just the builder, and
`unlogged_build_rel_info` is backend-local. v17 was fine because its flush path
opened the relation with persistence `0`:

```c
reln = smgropen(BufTagGetRelFileLocator(&buf->tag), INVALID_PROC_NUMBER, 0);
```

which lands in Neon's `case 0:` branch, and that branch already falls back to
`mdexists()`. v18 computes and passes the real persistence instead:

```c
reln = smgropen(BufTagGetRelFileLocator(&buf->tag), INVALID_PROC_NUMBER,
                relpersistence);
```

so flushes now take the `RELPERSISTENCE_PERMANENT` branch, which only consulted
the backend-local build state. The zero-LSN pages hit the "is evicted with zero
LSN" PANIC; the GiST ones sailed past it and fed 1 into the last-written-LSN
cache, tripping `Assert(lsn >= WalSegMinSize)`.

The fix applies the check Neon already had to the branch v18 moved flushes onto:

```c
 case RELPERSISTENCE_PERMANENT:
-    if (RelFileInfoEquals(unlogged_build_rel_info, InfoFromSMgrRel(reln)))
+    if (RelFileInfoEquals(unlogged_build_rel_info, InfoFromSMgrRel(reln)) ||
+        (!debug_compare_local && mdexists(reln, forknum)))
```

The extra `mdexists()` is not a new cost: v17 performed one on *every* buffer
flush by virtue of taking the `case 0:` path.

## 4c. MultiXact offsets: v18 stores the next entry too

v18's `RecordNewMultiXact()` writes two SLRU entries per multixact, not one:

```c
*offptr = offset;                        /* offsets[multi] */
...
next_offset = offset + nmembers;
if (next_offset == 0)
    next_offset = 1;                     /* as in GetNewMultiXactId() */
*next_offptr = next_offset;              /* offsets[multi + 1] -- new in v18 */
```

`GetNewMultiXactId()` extends the SLRU by one further MXID to match
(`ExtendMultiXactOffset(result + 1)`). This lets the last multixact's member
count be derived from the SLRU alone.

`ingest_multixact_create()` only wrote `offsets[mid]`, so the pageserver's copy
was short by exactly the final entry -- every earlier entry was filled in by the
*following* create. It surfaces as a basebackup that differs from the compute's
data directory in four bytes:

```
pgdata   : ... 9f 35 07 00 | b2 35 07 00 | c6 35 07 00 | 00 00 00 00
restored : ... 9f 35 07 00 | b2 35 07 00 | 00 00 00 00 | 00 00 00 00
```

with `pg_control` agreeing on `NextMultiXactId=23831, NextMultiOffset=472518` --
0x000735c6 is the missing value, at entry 23831.

Ingestion now emits `MultixactOffsetCreatePair` when `mid` and `mid + 1` share a
page, and two ordinary `MultixactOffsetCreate` records when the next entry starts
a new page. One record per key per LSN matters: two writes to the same key at one
LSN produce "Key ... written twice at same LSN" from the pageserver.

`MultixactOffsetCreatePair` is declared *before* the `#[cfg(feature = "testing")]
Test` variant so that the bincode discriminants of the production variants are
unchanged and existing v14..v17 layer files still deserialize.

## 4d. A new checkpoint variant needs its arms adding everywhere

`CheckPoint` is a per-version enum (`enum_pgversion!`), so adding `V18` compiles
only where the code dispatches across all versions — `enum_pgversion_dispatch!`
covers itself. Two places instead match specific variants, because the field they
touch does not exist before v17:

```rust
if info == pg_constants::XLOG_PARAMETER_CHANGE {
    if let CheckPoint::V17(cp) = &mut self.checkpoint {   /* ...and V18? */
```

Those matched `V17` alone and fell through silently for `V18`, so a v18 timeline
kept whatever `wal_level` its checkpoint was created with, ignoring every
`XLOG_PARAMETER_CHANGE` and `XLOG_END_OF_RECOVERY` that followed. The failure is
quiet — nothing errors, and the checkpoint handed to basebackup is merely stale —
so it is worth grepping for `CheckPoint::V17` (and the other `enum_pgversion!`
types) after adding a version, rather than trusting the compiler to find them.

Both records are byte-identical between v17 and v18, so the v17 decoders serve
both; only the arm that receives the result is new.

## 5. Data checksums

`initdb`'s default flipped:

```c
/* v17 */ static bool data_checksums = false;
/* v18 */ static bool data_checksums = true;
```

The pageserver *synthesizes* pages that never passed through a buffer manager —
zero-filled gap blocks when a relation is extended, relations produced by a
`FILE_COPY` `CREATE DATABASE`. Those carry `pd_checksum = 0`, so a checksummed
cluster rejects them:

```
page verification failed, calculated checksum 60807 but expected 0
invalid page in block 7 of relation "base/5/16384"
```

`postgres_initdb` therefore passes `--no-data-checksums` on v18+, preserving
v14–v17 behaviour.

That covers the clusters Neon creates. It does not cover the clusters it is
*given*: `pg_basebackup` of a stock v18 cluster now carries checksums, and both
import paths used to accept it, with the damage surfacing much later as

```
psycopg2.errors.DataCorrupted: invalid page in block 3 of relation "base/5/2608"
```

— a message that says nothing about checksums, pointing at a page the pageserver
synthesized rather than at the import that doomed it. `ensure_no_data_checksums()`
now rejects `data_checksum_version != 0` where the control file is first decoded,
which is a single choke point for both callers (`import_file()` serves the datadir
and basebackup-tar paths; `ControlFile::new()` serves the S3 import), and says
what to do about it. This is the one place a v18 default reaches a user who did
nothing wrong, so it is worth an error that names the cause.

**This is a decision, not a fact.** Turning checksums on would require the
pageserver to compute `pd_checksum` for the pages it synthesizes. Note that the
fork has already removed the usual objection to enabling them — commit
*"don't force FPI if checksums are enabled"* decouples `XLogHintBitIsNeeded()`
from `DataChecksumsEnabled()`, so the hint-bit WAL amplification that makes
checksums expensive elsewhere does not apply here. Upstream v18 still has the
coupling; the Neon fork does not.

Worth revisiting deliberately rather than inheriting this default.

---

## 6. Smaller API changes

| change | effect |
|---|---|
| `latch.h` split, `waiteventset.h` added | `WaitLatch`/`WL_*` no longer arrive transitively; four files need an explicit `#include "storage/latch.h"` |
| `ReplicationSlotAcquire()` gained `error_if_invalid` | `walproposer_pg.c` passes `true`, mirroring core's `StartReplication()` |
| `INT64_MODIFIER` removed in favour of `<inttypes.h>` | `INT64_HEX_FORMAT`/`UINT64_HEX_FORMAT` use `PRIx64` on v18 |
| `relpath()` returns `RelPathStr` | `.str` needed at the call site |
| `int64` is `long long` | `%ld` → `INT64_FORMAT` |
| reorderbuffer `Get`/`Return{TupleBuf,Change}` renamed to `Alloc`/`Free` | compat macros let the v17 `neon_rmgr` decoder serve v18 unchanged |
| `xl_heap_inplace` gained invalidation messages | not parsed by Neon's Rust decoders; no action |

### The last-written-LSN hooks were removed on purpose

`set_lwlsn_block_hook`, `set_lwlsn_block_range_hook` and `set_lwlsn_block_v_hook`
are absent from the v18 branch. That is deliberate, not an omission — commit
*"Remove unnecessary set_lwlsn_block_* hooks"* explains that the only caller,
`gist_indexsortbuild()`, was redundant with the `smgr_bulk_*()` write path, and
the other two were never called by core.

`neon_lwlsncache` therefore no longer installs them; the `neon_set_lwlsn_block*()`
functions remain because `pagestore_smgr.c` calls them directly. The
`LastWrittenLsn` LWLock also went with them, so the extension allocates its own
named tranche.

---

## 7. Extensions

Versions follow Neon's published supported-extensions table where it has an
opinion. Every version was downloaded and its sha256 computed locally, and the
plain-PGXS ones were compiled against a real PostgreSQL 18 tree.

**Bumped because the pinned version does not build against v18:**

| extension | | breakage |
|---|---|---|
| pgvector | 0.8.0 → 0.8.1 | `vacuum_delay_point()` gained an argument |
| plpgsql_check | 2.7.11 → 2.8.2 | `PLpgSQL_func_hashkey` gone, `use_count` removed |
| pg_ivm | 1.9 → 1.12 | `ExecutorRun`/`CheckIndexCompatible` signatures |
| pg_cron | 1.6.4 → 1.6.6 | `PortalRun()` signature |
| pg_anon | 2.1.0 → 2.5.1 | 2.1.0 only declares features `pg13`..`pg17` |

**Bumped to follow the release series:** postgis 3.5.0 → 3.6.0, pgrouting 3.6.2 →
3.8.0, rdkit → `Release_2025_09_6` (cartridge 4.8.0), pg_hint_plan
`REL17_1_7_0` → `REL18_1_8_0`, pgaudit 17.1 → 18.0, pg_semver 0.40.0 → 0.41.0,
timescaledb 2.17.1 → 2.23.0, h3-pg 4.1.3 → 4.2.3.

**pgrx.** pgrx only gained PostgreSQL 18 support in 0.15.0, and the three
toolchains here were pinned to 0.11.3, 0.12.9 and 0.14.1. Every extension release
that supports v18 pins exactly **0.16.1**, so one toolchain covers all of them —
the stages select 0.16.1 on v18 and keep their existing pins elsewhere. No new
stage, and no conditional `FROM`, which Docker cannot express.

Each such extension also needs `unsafe-postgres` added to its pgrx dependency:
pgrx refuses to build against a fork that reports a custom ABI name, which Neon's
Postgres does.

**Not built on v18:** `rum` (no upstream support — its `PostingItem` conflicts
with core), `plv8` (deprecated), `pg_mooncake`, `pg_duckdb`, `online_advisor`.
Neon does not ship the first three on PG18 either. `pgrag` was removed outright
in its own commit.

`pg_duckdb` is the one worth revisiting, because it is a version bump rather
than a port. The pinned v0.3.1 (Feb 2025) predates v18 and fails on four
separate API changes:

```
'POSIX_COLLATION_OID' was not declared in this scope
'ExplainPropertyText' was not declared in this scope
'MemoryContextReset' was not declared in this scope
cannot convert 'Node*' to 'Query*' in assignment
```

Note that gcc suggests `C_COLLATION_OID` for the first one and that suggestion is
wrong: v18 kept the POSIX collation (OID 951 is still in `pg_collation.dat`) and
only dropped its `oid_symbol`, so substituting `C_COLLATION_OID` (OID 950) would
change behaviour rather than restore it.

pg_duckdb **v1.1.1 supports 14..18**, so the fix is to bump. It is not a drop-in,
though — both local patches assume v0.3.1:

- `pg_duckdb_v031.patch` renames `libduckdb` to `libduckdb_pg_duckdb` so
  pg_duckdb (duckdb 1.2.0) and pg_mooncake (duckdb 1.1.3) can coexist in one
  backend, and appends the `neon.privileged_role_name` GRANTs to
  `sql/pg_duckdb--0.2.0--0.3.0.sql`. The 1.x SQL layout replaces that file with
  `pg_duckdb--1.0.0.sql` plus `pg_duckdb--1.0.0--1.1.0.sql`.
- `duckdb_v120.patch` renames the CMake target inside the vendored duckdb 1.2.0,
  which 1.1.1 does not vendor.

Since pg_mooncake is already skipped on v18, the symbol-collision that motivates
the rename does not arise there, which should simplify the rebase.

`pg_mooncake` earns its place on that list from two v18 API breaks in its
columnstore table AM, which only surface once the stage gets far enough to
compile `src/columnstore_handler.cpp`:

```
columnstore_handler.cpp:147: 'struct TupleDescData' has no member named 'attrs'
columnstore_handler.cpp:295: too many initializers for 'const TableAmRoutine'
```

v18 replaced `TupleDescData.attrs[]` with `compact_attrs[]` (reached via
`TupleDescAttr()`), and dropped members from `TableAmRoutine` in the bitmap heap
scan rework. 0.1.2 (Feb 2025) is still the newest release, so there is no version
to bump to — supporting it would mean porting a third-party table AM across both
changes. It is gated behind `neon.unstable_extensions` regardless.

**Traps worth knowing:**

- **pgaudit 18.0 already contains** the parallel-worker patch carried for
  v14–v17, so applying it fails as *"Reversed (or previously applied)"*. v18 skips
  it; a placeholder patch file keeps the unconditional `COPY` resolving.
- **rdkit needs Boost ≥ 1.81** (`RDK_BOOST_VERSION`) from `Release_2025_03`
  onwards, and bookworm ships 1.74. There is no rdkit that both supports v18 and
  works with stock bookworm Boost, so v18 pulls 1.83 from `bookworm-backports`.
- **h3-pg pins its core library version.** 4.1.3 wants H3 4.1.0, 4.2.3 wants
  4.2.0, so the C library is version-cased alongside it.
- **pgrx sed placement.** pgrag's pins live in `exts/<name>/Cargo.toml`, pg_anon's
  one directory down from the build's working directory, and pg_session_jwt 0.5.0
  declares `pgrx-tests` as a *path* dependency that must not be rewritten.

---

## 8. Bugs found that have nothing to do with PostgreSQL 18

Building and testing surfaced three pre-existing problems:

1. **`h3-pg` moved from `zachasme/h3-pg` to the `postgis` org** and the tags went
   with it, so the pinned URL 404s for *every* Postgres version. Contents are
   identical — the v4.1.3 checksum is unchanged.
2. **`timescaledb`'s version `case` had no `*)` fallback**, so an unrecognised
   version left `TIMESCALEDB_VERSION` unset and `wget` fetched a nonsense URL
   instead of failing with a clear message.
3. **`endpoint_storage` never exits on SIGTERM.** It receives the signal, logs
   that it is shutting down, cancels its token — and then hangs, because axum's
   `with_graceful_shutdown` waits for connections to drain. Measured: still alive
   41s after SIGTERM, with no clients connected. `neon_local` allows 10s, so every
   test that stops the service fails in teardown and leaks processes until ports
   run out. The drain now has a bounded budget.

---

## 9. What is not done

- **Asynchronous I/O.** `neon_startreadv()` completes reads inline. Genuine async
  would mean issuing pageserver requests concurrently and completing the AIO
  handle later. Neon's own v18 is in the same position.
- **Data checksums** are disabled; see §5.
- **`walproposer-lib`** is still built against v17 only. That is a deliberate
  single-version choice predating this work, not a v18 gap.
- **Extension coverage**: see the "not built on v18" list above.

---

## 10. Reproducing the checks

The two format findings came from compiling a probe against both header sets,
which is worth repeating whenever a major version lands:

```c
#include "postgres.h"
#include "access/xact.h"
#include "catalog/pg_control.h"
#include <stdio.h>
#undef printf
int main(void) {
    printf("sizeof(xl_xact_stats_item) %zu\n", sizeof(xl_xact_stats_item));
    printf("offsetof(ControlFileData, crc) %zu\n", offsetof(ControlFileData, crc));
    return 0;
}
```

```sh
for v in v17 v18; do
  clang -I"$(pg_install/$v/bin/pg_config --includedir-server)" -o /tmp/p_$v probe.c && /tmp/p_$v
done
```

To compile an extension against the v18 tree without building the whole image:

```sh
make USE_PGXS=1 PG_CONFIG=pg_install/v18/bin/pg_config -j4
```

### The places a new version does not announce itself

Most of the port is found by the compiler. These are not, and each one was a real
miss here — worth walking after adding a version:

```sh
# Variant matches that silently skip the new version (§4d).
rg 'CheckPoint::V17|::V17\(' --type rust

# Version ranges written as patterns rather than as a list.
rg '1\[4-[0-9]\]|1[4-7] *\||"1[4-7]"' --type json --type yaml --type rust

# Control files decoded without going through decode_control_file() (§2.2).
rg 'ControlFileData::decode\b' --type rust

# Per-version settings the control plane reads but this repo never loads.
rg -n 'per_version' compute/manifest.yaml compute/manifest.schema.json
```

The pattern they share: the new version parses, compiles and runs, and simply
does not get the behaviour — a stale `wal_level`, a missing `io_method`, a control
file declared corrupt. None of them fail loudly, so none of them show up in a
green test run.
