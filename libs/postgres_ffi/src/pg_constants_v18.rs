use crate::PgMajorVersion;

pub const MY_PGVERSION: PgMajorVersion = PgMajorVersion::PG18;

pub const XACT_XINFO_HAS_DROPPED_STATS: u32 = 1u32 << 8;

pub const XLOG_DBASE_CREATE_FILE_COPY: u8 = 0x00;
pub const XLOG_DBASE_CREATE_WAL_LOG: u8 = 0x10;
pub const XLOG_DBASE_DROP: u8 = 0x20;

pub const BKPIMAGE_APPLY: u8 = 0x02; /* page image should be restored during replay */
pub const BKPIMAGE_COMPRESS_PGLZ: u8 = 0x04; /* page image is compressed */
pub const BKPIMAGE_COMPRESS_LZ4: u8 = 0x08; /* page image is compressed */
pub const BKPIMAGE_COMPRESS_ZSTD: u8 = 0x10; /* page image is compressed */

pub const SIZEOF_RELMAPFILE: usize = 524; /* sizeof(RelMapFile) in relmapper.c */

/// sizeof(xl_xact_stats_item) in src/include/access/xact.h.
///
/// PostgreSQL 18 replaced the single `Oid objoid` field with a 64-bit object ID
/// split into `uint32 objid_lo` and `uint32 objid_hi`, which grew the struct
/// from 12 to 16 bytes (see PostgreSQL commit that widened PgStat_HashKey.objid).
pub const SIZEOF_XL_XACT_STATS_ITEM: usize = 16;

// The list of subdirectories inside pgdata is unchanged from v17.
pub use super::super::v17::bindings::PGDATA_SUBDIRS;

pub fn bkpimg_is_compressed(bimg_info: u8) -> bool {
    const ANY_COMPRESS_FLAG: u8 =
        BKPIMAGE_COMPRESS_PGLZ | BKPIMAGE_COMPRESS_LZ4 | BKPIMAGE_COMPRESS_ZSTD;

    (bimg_info & ANY_COMPRESS_FLAG) != 0
}

pub const XLOG_HEAP2_PRUNE_ON_ACCESS: u8 = 0x10;
pub const XLOG_HEAP2_PRUNE_VACUUM_SCAN: u8 = 0x20;
pub const XLOG_HEAP2_PRUNE_VACUUM_CLEANUP: u8 = 0x30;

pub const XLOG_OVERWRITE_CONTRECORD: u8 = 0xD0;
pub const XLOG_CHECKPOINT_REDO: u8 = 0xE0;
