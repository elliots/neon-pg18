//!
//! Utilities for reading and writing the PostgreSQL control file.
//!
//! The PostgreSQL control file is one the first things that the PostgreSQL
//! server reads when it starts up. It indicates whether the server was shut
//! down cleanly, or if it crashed or was restored from online backup so that
//! WAL recovery needs to be performed. It also contains a copy of the latest
//! checkpoint record and its location in the WAL.
//!
//! The control file also contains fields for detecting whether the
//! data directory is compatible with a postgres binary. That includes
//! a version number, configuration options that can be set at
//! compilation time like the block size, and the platform's alignment
//! and endianness information. (The PostgreSQL on-disk file format is
//! not portable across platforms.)
//!
//! The control file is stored in the PostgreSQL data directory, as
//! `global/pg_control`. The data stored in it is designed to be smaller than
//! 512 bytes, on the assumption that it can be updated atomically. The actual
//! file is larger, 8192 bytes, but the rest of it is just filled with zeros.
//!
//! See src/include/catalog/pg_control.h in the PostgreSQL sources for more
//! information. You can use PostgreSQL's pg_controldata utility to view its
//! contents.
//!
use super::bindings::{ControlFileData, PG_CONTROL_FILE_SIZE};

use anyhow::{bail, Result};
use bytes::{Bytes, BytesMut};

/// Equivalent to sizeof(ControlFileData) in C
const SIZEOF_CONTROLDATA: usize = size_of::<ControlFileData>();

impl ControlFileData {
    /// Compute the offset of the `crc` field within the `ControlFileData` struct.
    /// Equivalent to offsetof(ControlFileData, crc) in C.
    pub const fn pg_control_crc_offset() -> usize {
        std::mem::offset_of!(ControlFileData, crc)
    }

    ///
    /// Interpret a slice of bytes as a Postgres control file.
    ///
    pub fn decode(buf: &[u8]) -> Result<ControlFileData> {
        Self::decode_with_crc_offset(buf, Self::pg_control_crc_offset())
    }

    ///
    /// Interpret a slice of bytes as a Postgres control file, taking the offset
    /// of the `crc` field from the caller.
    ///
    /// The position of `crc` is version-dependent: PostgreSQL 18 inserted a
    /// `default_char_signedness` field ahead of `mock_authentication_nonce`,
    /// which moved `crc` from offset 288 to 292. Code that reads a control file
    /// belonging to a possibly-different major version should go through
    /// [`crate::decode_control_file`], which picks the offset based on the
    /// `pg_control_version` recorded in the file itself.
    ///
    pub fn decode_with_crc_offset(buf: &[u8], offsetof_crc: usize) -> Result<ControlFileData> {
        use utils::bin_ser::LeSer;

        // Check that the slice has the expected size. The control file is
        // padded with zeros up to a 512 byte sector size, so accept a
        // larger size too, so that the caller can just the whole file
        // contents without knowing the exact size of the struct.
        if buf.len() < SIZEOF_CONTROLDATA {
            bail!("control file is too short");
        }
        if offsetof_crc + size_of::<u32>() > buf.len() {
            bail!("control file is too short to contain a CRC at offset {offsetof_crc}");
        }

        // Compute the expected CRC of the content.
        let expectedcrc = crc32c::crc32c(&buf[0..offsetof_crc]);

        // Read the stored CRC straight from the buffer rather than from the
        // deserialized struct, since the struct layout might belong to a
        // different major version than the file.
        let actualcrc = u32::from_ne_bytes(
            buf[offsetof_crc..offsetof_crc + size_of::<u32>()]
                .try_into()
                .unwrap(),
        );

        // Check the CRC
        if expectedcrc != actualcrc {
            bail!(
                "invalid CRC in control file: expected {:08X}, was {:08X}",
                expectedcrc,
                actualcrc
            );
        }

        // Use serde to deserialize the input as a ControlFileData struct.
        let mut controlfile = ControlFileData::des_prefix(buf)?;

        // Make sure the returned struct reports the CRC we actually validated,
        // even if this struct's layout puts `crc` somewhere else.
        controlfile.crc = actualcrc;

        Ok(controlfile)
    }

    ///
    /// Convert a struct representing a Postgres control file into raw bytes.
    ///
    /// The CRC is recomputed to match the contents of the fields.
    pub fn encode(&self) -> Bytes {
        use utils::bin_ser::LeSer;

        // Serialize into a new buffer.
        let b = self.ser().unwrap();

        // Recompute the CRC
        let OFFSETOF_CRC = Self::pg_control_crc_offset();
        let newcrc = crc32c::crc32c(&b[0..OFFSETOF_CRC]);

        let mut buf = BytesMut::with_capacity(PG_CONTROL_FILE_SIZE as usize);
        buf.extend_from_slice(&b[0..OFFSETOF_CRC]);
        buf.extend_from_slice(&newcrc.to_ne_bytes());
        // Fill the rest of the control file with zeros.
        buf.resize(PG_CONTROL_FILE_SIZE as usize, 0);

        buf.into()
    }
}
