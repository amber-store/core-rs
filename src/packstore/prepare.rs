//! The write paths' common front: turning an [`Object`] into the record to
//! append, whichever form it arrived in (Go: `packstore/prepare.go`).

use crate::amberpack::{REC_HEADER_SIZE, Record, decode_payload, encode_record, parse_record};
use crate::key::Key;

use super::verify::verify_object;
use super::{Error, Object, corrupt};

/// Returns the record to append for `obj` and the payload length the write
/// stats charge for it. For `data` that is [`encode_record`]'s output after
/// the optional verification; for a pre-encoded `record` it is the record
/// itself, after [`check_record`]. Every rejection of a record is a
/// corrupt-class error, a verification failure a verify-class one (Go:
/// `prepare`).
pub(super) fn prepare(obj: Object, verify: bool) -> Result<(Vec<u8>, u64), Error> {
    let Some(record) = obj.record else {
        if verify {
            verify_object(obj.key, &obj.data).map_err(Error::Verify)?;
        }
        let rec = encode_record(obj.key, &obj.data).map_err(Error::Pack)?;
        return Ok((rec, obj.data.len() as u64));
    };
    if !obj.data.is_empty() {
        return Err(corrupt(format!(
            "object {} carries both Data and Record",
            obj.key
        )));
    }
    let rec = check_record(obj.key, &record, verify)?;
    Ok((record, u64::from(rec.ulen)))
}

/// Validates a pre-encoded record offered for `k`: [`parse_record`]
/// (framing, flags, length invariants, CRC, canonical key), a check that the
/// record names `k` and is exactly one record long, and, with `verify`, a
/// decode and rehash of the payload. Returns the parsed header. Shared by
/// [`prepare`] and [`Store::append_record`](super::Store::append_record),
/// which borrows its record rather than handing over an [`Object`] (Go: the
/// `Record` half of `prepare`).
pub(super) fn check_record(k: Key, raw: &[u8], verify: bool) -> Result<Record, Error> {
    let rec = parse_record(raw).map_err(Error::Pack)?;
    if rec.key != k {
        return Err(corrupt(format!(
            "record key {} does not match {}",
            rec.key, k
        )));
    }
    if raw.len() != REC_HEADER_SIZE + rec.slen as usize {
        return Err(corrupt(format!(
            "record is {} bytes, want {}",
            raw.len(),
            REC_HEADER_SIZE + rec.slen as usize
        )));
    }
    if verify {
        let data =
            decode_payload(rec.flags, rec.ulen, &raw[REC_HEADER_SIZE..]).map_err(Error::Pack)?;
        verify_object(k, &data).map_err(Error::Verify)?;
    }
    Ok(rec)
}
