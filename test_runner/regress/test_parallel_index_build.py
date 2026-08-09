from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from fixtures.neon_fixtures import NeonEnv


#
# Build indexes with parallel workers, for the access methods that use the
# unlogged-build path.
#
# smgr_start_unlogged_build() sends an index build's writes to local storage and
# the access method WAL-logs the finished index afterwards. Neon tracks that with
# backend-local state (unlogged_build_rel_info in pagestore_smgr.c), and only the
# leader calls smgr_start_unlogged_build() -- a parallel worker writing index
# pages does not go through it.
#
# That only became reachable for GIN in v18, which added parallel GIN builds
# (_gin_parallel_build_main); v17's gininsert.c has no parallel support at all.
# btree has had parallel builds for much longer and uses bulk_write.c rather than
# the unlogged-build hooks, so it is included here as the control case.
#
# Nothing else in test_runner sets max_parallel_maintenance_workers, so without
# this the parallel build paths are never exercised against Neon storage.
#
def test_parallel_index_build(neon_simple_env: NeonEnv):
    env = neon_simple_env
    endpoint = env.endpoints.create_start(
        "main",
        config_lines=[
            "max_parallel_maintenance_workers = 4",
            "max_parallel_workers = 8",
            "max_worker_processes = 16",
            # The planner only picks parallel workers once the input looks big
            # enough; keep the thresholds low so this stays quick.
            "min_parallel_table_scan_size = 0",
            "maintenance_work_mem = '96MB'",
        ],
    )

    with endpoint.connect() as conn:
        cur = conn.cursor()

        cur.execute("""
            CREATE TABLE parallel_idx_tbl AS
                SELECT g AS id,
                       ARRAY[g % 100, g % 971, g % 7919] AS arr,
                       md5(g::text) AS txt
                FROM generate_series(1, 200000) g
        """)

        # Each of these should be able to recruit workers. Creating the index in
        # its own transaction after the table exists is the ordinary case; the
        # same-transaction case is covered below.
        cur.execute("CREATE INDEX parallel_gin_idx ON parallel_idx_tbl USING gin (arr)")
        cur.execute("CREATE INDEX parallel_btree_idx ON parallel_idx_tbl (id)")

        # Same transaction as the table's creation, which is what puts Neon on
        # the unlogged-build path: the relation is new, so writes go to local
        # storage until the access method WAL-logs the finished index.
        cur.execute("BEGIN")
        cur.execute("""
            CREATE TABLE parallel_idx_tbl2 AS
                SELECT g AS id, ARRAY[g % 100, g % 971] AS arr
                FROM generate_series(1, 200000) g
        """)
        cur.execute("CREATE INDEX parallel_gin_idx2 ON parallel_idx_tbl2 USING gin (arr)")
        cur.execute("COMMIT")

        # Force everything out of shared buffers so the pages are read back from
        # the pageserver rather than answered from cache.
        cur.execute("CHECKPOINT")

        # The indexes have to actually return the right rows afterwards.
        cur.execute("SET enable_seqscan = off")
        for table in ("parallel_idx_tbl", "parallel_idx_tbl2"):
            cur.execute(f"SELECT count(*) FROM {table} WHERE arr @> ARRAY[42]")
            via_index = cur.fetchall()[0][0]
            cur.execute("SET enable_seqscan = on")
            cur.execute(f"SELECT count(*) FROM {table} WHERE arr @> ARRAY[42]")
            via_seqscan = cur.fetchall()[0][0]
            cur.execute("SET enable_seqscan = off")
            assert via_index == via_seqscan, (
                f"{table}: GIN index returned {via_index} rows, sequential scan {via_seqscan}"
            )
            assert via_index > 0, f"{table}: expected the query to match some rows"

        cur.execute("SELECT count(*) FROM parallel_idx_tbl WHERE id BETWEEN 1000 AND 2000")
        assert cur.fetchall()[0][0] == 1001

    # Restarting drops every cached page, so this reads the indexes back out of
    # the pageserver from scratch.
    endpoint.stop()
    endpoint.start()

    with endpoint.connect() as conn:
        cur = conn.cursor()
        cur.execute("SET enable_seqscan = off")
        cur.execute("SELECT count(*) FROM parallel_idx_tbl WHERE arr @> ARRAY[42]")
        assert cur.fetchall()[0][0] > 0
        cur.execute("SELECT count(*) FROM parallel_idx_tbl2 WHERE arr @> ARRAY[42]")
        assert cur.fetchall()[0][0] > 0
        cur.execute("SELECT count(*) FROM parallel_idx_tbl WHERE id BETWEEN 1000 AND 2000")
        assert cur.fetchall()[0][0] == 1001
