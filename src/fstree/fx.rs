//! A faithful Rust port of the subset of `fxamacker/cbor` v2.9.2 behavior
//! that the Go `fstree` decoders and encoders rely on.
//!
//! Go's `fstree/decode.go` calls `cbor.Unmarshal` with the **default** decode
//! mode, so the exact acceptance behavior of this crate's decoders — what is
//! validated, what is silently tolerated, and every diagnostic string — must
//! reproduce fxamacker's. That behavior was established by reading the
//! library source (`valid.go`, `decode.go` at v2.9.2, the version pinned in
//! the Go module) and cross-checked against a differential oracle harness run
//! against the real Go implementation. The relevant defaults:
//!
//! * Unmarshal first checks the entire input for **well-formedness** (with
//!   the extraneous-data check), then type-decodes; syntax errors therefore
//!   win over type errors.
//! * Indefinite-length items, non-shortest heads, and CBOR tags are all
//!   accepted on decode; tags are unwrapped (with special handling for bignum
//!   tags 2/3 and content-type validation of built-in tags 0–3).
//! * Limits: max nesting 32, max array elements 131072, max map pairs 131072.
//! * Struct decoding (Go `keyasint`): unknown keys are skipped, duplicate
//!   keys keep the first value and skip the rest without type-checking, text
//!   keys never match integer-keyed fields, and non-integer/non-text keys
//!   produce "cannot be used to match struct field name" errors.
//! * `cbor.RawMessage` output is re-validated on encode with the encoder's
//!   options (indefinite lengths forbidden, built-in tag content checked,
//!   limits maxed out); [`raw_message_wellformed`] mirrors that check.
//!
//! Diagnostics render byte-for-byte like fxamacker's errors, including Go
//! type names (`[]uint8`, `fstree.Entry.4`, …), so wrapped fstree errors
//! compare equal to the Go implementation's strings.

use std::fmt;

use super::{DirPair, Entry};
use crate::commit::{WireCommit, WireIdentity};

/// CBOR major-type names exactly as fxamacker/cbor renders them in
/// diagnostics (`cborType.String()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CborType {
    /// Major 0.
    PositiveInt,
    /// Major 1.
    NegativeInt,
    /// Major 2.
    ByteString,
    /// Major 3.
    TextString,
    /// Major 4.
    Array,
    /// Major 5.
    Map,
    /// Major 6.
    Tag,
    /// Major 7.
    Primitives,
}

impl CborType {
    fn from_major(m: u8) -> CborType {
        match m & 7 {
            0 => CborType::PositiveInt,
            1 => CborType::NegativeInt,
            2 => CborType::ByteString,
            3 => CborType::TextString,
            4 => CborType::Array,
            5 => CborType::Map,
            6 => CborType::Tag,
            _ => CborType::Primitives,
        }
    }

    fn from_initial_byte(b: u8) -> CborType {
        CborType::from_major(b >> 5)
    }

    /// The fxamacker diagnostic name.
    pub fn name(self) -> &'static str {
        match self {
            CborType::PositiveInt => "positive integer",
            CborType::NegativeInt => "negative integer",
            CborType::ByteString => "byte string",
            CborType::TextString => "UTF-8 text string",
            CborType::Array => "array",
            CborType::Map => "map",
            CborType::Tag => "tag",
            CborType::Primitives => "primitives",
        }
    }
}

impl fmt::Display for CborType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A CBOR decode (or `RawMessage` re-validation) failure, reproducing the
/// exact diagnostic strings of fxamacker/cbor v2.9.2 — including its Go type
/// names — so errors wrapped by the fstree decoders match the Go
/// implementation byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CborError {
    /// Go `io.EOF`: the input was empty.
    Eof,
    /// Go `io.ErrUnexpectedEOF`: the input ended mid-item.
    UnexpectedEof,
    /// `cbor: unexpected "break" code`
    UnexpectedBreak,
    /// `cbor: invalid additional information <ai> for type <t>`
    InvalidAdditionalInformation {
        /// The offending additional-information bits (28–31).
        ai: u8,
        /// The head's major type.
        t: CborType,
    },
    /// `cbor: invalid simple value <n> for type primitives` (two-byte simple
    /// values 24–31 are reserved).
    InvalidSimpleValue(u8),
    /// `cbor: <t> length <len> is too large, causing integer overflow`
    /// (byte/text string length above `i64::MAX`).
    StringLengthOverflow {
        /// ByteString or TextString.
        t: CborType,
        /// The claimed length.
        len: u64,
    },
    /// `cbor: <t> length <len> is too large, it would cause integer overflow`
    /// (array/map count above `i64::MAX`; note the different phrasing).
    ContainerLengthOverflow {
        /// Array or Map.
        t: CborType,
        /// The claimed count.
        len: u64,
    },
    /// `cbor: exceeded max nested level <n>`
    MaxNestedLevel(usize),
    /// `cbor: exceeded max number of elements <n> for CBOR array`
    MaxArrayElements(usize),
    /// `cbor: exceeded max number of key-value pairs <n> for CBOR map`
    MaxMapPairs(usize),
    /// `cbor: wrong element type <chunk> for indefinite-length <t>`
    WrongIndefiniteChunkType {
        /// The string type being assembled.
        t: CborType,
        /// The offending chunk's type.
        chunk: CborType,
    },
    /// `cbor: indefinite-length <t> chunk is not definite-length`
    IndefiniteChunkNotDefinite(CborType),
    /// `cbor: indefinite-length <t> isn't allowed` (encode-side `RawMessage`
    /// validation; the deterministic encoder forbids indefinite lengths).
    IndefiniteLengthNotAllowed(CborType),
    /// `cbor: <num> bytes of extraneous data starting at index <index>`
    ExtraneousData {
        /// Number of trailing bytes.
        num: usize,
        /// Offset where they start.
        index: usize,
    },
    /// `cbor: invalid UTF-8 string`
    InvalidUtf8,
    /// `cbor: tag number <tag> must be followed by <expected>, got <got>` —
    /// built-in tag (0–3) content-type validation.
    InadmissibleTagContent {
        /// `"0"`, `"1"`, or `"2 or 3"` exactly as Go renders it.
        tag: &'static str,
        /// The expected content description.
        expected: &'static str,
        /// The actual content type.
        got: CborType,
    },
    /// `cbor: cannot unmarshal <t> into Go value of type <go_type>`, or with
    /// a struct field context `... into Go struct field <field> of type
    /// <go_type>`, optionally suffixed ` (<msg>)`.
    UnmarshalType {
        /// The CBOR value's type.
        t: CborType,
        /// The Go type name of the destination (e.g. `[]uint8`).
        go_type: &'static str,
        /// `fstree.Entry.<key>` / `fstree.DirPair.<field>` context, if any.
        field: Option<&'static str>,
        /// Extra detail, e.g. `18446744073709551615 overflows int64`.
        msg: Option<String>,
    },
}

impl fmt::Display for CborError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CborError::Eof => f.write_str("EOF"),
            CborError::UnexpectedEof => f.write_str("unexpected EOF"),
            CborError::UnexpectedBreak => f.write_str("cbor: unexpected \"break\" code"),
            CborError::InvalidAdditionalInformation { ai, t } => {
                write!(f, "cbor: invalid additional information {ai} for type {t}")
            }
            CborError::InvalidSimpleValue(n) => {
                write!(f, "cbor: invalid simple value {n} for type primitives")
            }
            CborError::StringLengthOverflow { t, len } => {
                write!(
                    f,
                    "cbor: {t} length {len} is too large, causing integer overflow"
                )
            }
            CborError::ContainerLengthOverflow { t, len } => {
                write!(
                    f,
                    "cbor: {t} length {len} is too large, it would cause integer overflow"
                )
            }
            CborError::MaxNestedLevel(n) => write!(f, "cbor: exceeded max nested level {n}"),
            CborError::MaxArrayElements(n) => {
                write!(
                    f,
                    "cbor: exceeded max number of elements {n} for CBOR array"
                )
            }
            CborError::MaxMapPairs(n) => {
                write!(
                    f,
                    "cbor: exceeded max number of key-value pairs {n} for CBOR map"
                )
            }
            CborError::WrongIndefiniteChunkType { t, chunk } => {
                write!(
                    f,
                    "cbor: wrong element type {chunk} for indefinite-length {t}"
                )
            }
            CborError::IndefiniteChunkNotDefinite(t) => {
                write!(
                    f,
                    "cbor: indefinite-length {t} chunk is not definite-length"
                )
            }
            CborError::IndefiniteLengthNotAllowed(t) => {
                write!(f, "cbor: indefinite-length {t} isn't allowed")
            }
            CborError::ExtraneousData { num, index } => {
                write!(
                    f,
                    "cbor: {num} bytes of extraneous data starting at index {index}"
                )
            }
            CborError::InvalidUtf8 => f.write_str("cbor: invalid UTF-8 string"),
            CborError::InadmissibleTagContent { tag, expected, got } => {
                write!(
                    f,
                    "cbor: tag number {tag} must be followed by {expected}, got {got}"
                )
            }
            CborError::UnmarshalType {
                t,
                go_type,
                field,
                msg,
            } => {
                match field {
                    Some(fld) => write!(
                        f,
                        "cbor: cannot unmarshal {t} into Go struct field {fld} of type {go_type}"
                    )?,
                    None => write!(
                        f,
                        "cbor: cannot unmarshal {t} into Go value of type {go_type}"
                    )?,
                }
                if let Some(m) = msg {
                    write!(f, " ({m})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for CborError {}

/// Well-formedness options (fxamacker `decMode` fields that matter here).
pub(super) struct WfOpts {
    max_nested: usize,
    max_array: u64,
    max_map: u64,
    indef_allowed: bool,
    check_builtin_tags: bool,
}

/// fxamacker's default decode options, used by `cbor.Unmarshal`.
pub(super) const DECODE_WF: WfOpts = WfOpts {
    max_nested: 32,
    max_array: 131072,
    max_map: 131072,
    indef_allowed: true,
    check_builtin_tags: false,
};

/// The mode fxamacker uses to validate `Marshaler` output (`RawMessage`) from
/// a core-deterministic encoder: limits maxed out, indefinite lengths
/// forbidden, built-in tag content checked.
pub(super) const MARSHALER_WF: WfOpts = WfOpts {
    max_nested: 65535,
    max_array: 2147483647,
    max_map: 2147483647,
    indef_allowed: false,
    check_builtin_tags: true,
};

/// Validates a spliced `RawMessage`: exactly one well-formed CBOR item under
/// the encoder's restrictions (mirrors `encodeMarshalerType`'s check).
pub(super) fn raw_message_wellformed(data: &[u8]) -> Result<(), CborError> {
    wellformed(data, &MARSHALER_WF)
}

/// Port of fxamacker `decoder.wellformed(false, …)`: checks that `data` is
/// exactly one well-formed CBOR item with nothing trailing.
pub(super) fn wellformed(data: &[u8], opts: &WfOpts) -> Result<(), CborError> {
    if data.is_empty() {
        return Err(CborError::Eof);
    }
    let mut off = 0usize;
    wf_item(data, &mut off, opts)?;
    if off != data.len() {
        return Err(CborError::ExtraneousData {
            num: data.len() - off,
            index: off,
        });
    }
    Ok(())
}

/// Container state while checking well-formedness (iterative equivalent of
/// fxamacker's recursion, so the marshaler mode's 65535-level limit cannot
/// exhaust the thread stack).
enum Frame {
    /// A definite-length array or map with `remaining` items still expected
    /// (a map counts key and value separately).
    Definite { remaining: u64, depth: usize },
    /// An indefinite-length array (`map == false`) or map, with the number of
    /// items seen so far.
    Indefinite { map: bool, count: u64, depth: usize },
    /// An indefinite-length byte/text string awaiting chunks.
    Chunks(CborType),
}

fn wf_item(data: &[u8], off: &mut usize, opts: &WfOpts) -> Result<(), CborError> {
    let mut stack: Vec<Frame> = Vec::new();
    // Set when the item about to be parsed is a tag's content: carries the
    // (possibly chain-incremented) depth and suppresses the parent frame's
    // break-flag handling, exactly like Go's direct recursion after the tag
    // chain scan.
    let mut tag_content_depth: Option<usize> = None;

    loop {
        if tag_content_depth.is_none() {
            match stack.last() {
                Some(Frame::Chunks(t)) => {
                    let t = *t;
                    if *off == data.len() {
                        return Err(CborError::UnexpectedEof);
                    }
                    if data[*off] == 0xff {
                        *off += 1;
                        stack.pop();
                        if complete_item(&mut stack, opts)? {
                            return Ok(());
                        }
                        continue;
                    }
                    let b = data[*off];
                    let chunk = CborType::from_initial_byte(b);
                    if chunk != t {
                        return Err(CborError::WrongIndefiniteChunkType { t, chunk });
                    }
                    if b & 0x1f == 31 {
                        return Err(CborError::IndefiniteChunkNotDefinite(t));
                    }
                }
                Some(Frame::Indefinite { map, count, .. }) => {
                    if *off == data.len() {
                        return Err(CborError::UnexpectedEof);
                    }
                    if data[*off] == 0xff {
                        let odd = *map && *count % 2 == 1;
                        *off += 1;
                        if odd {
                            // Key without value before the break.
                            return Err(CborError::UnexpectedBreak);
                        }
                        stack.pop();
                        if complete_item(&mut stack, opts)? {
                            return Ok(());
                        }
                        continue;
                    }
                }
                _ => {}
            }
        }

        // Base depth for the item being parsed.
        let mut depth = tag_content_depth
            .take()
            .unwrap_or_else(|| match stack.last() {
                Some(Frame::Definite { depth, .. }) | Some(Frame::Indefinite { depth, .. }) => {
                    *depth
                }
                _ => 0,
            });

        let (t, ai, val) = wf_head(data, off)?;
        let indef = ai == 31;

        match t {
            CborType::PositiveInt | CborType::NegativeInt | CborType::Primitives => {
                if complete_item(&mut stack, opts)? {
                    return Ok(());
                }
            }
            CborType::ByteString | CborType::TextString => {
                if indef {
                    if !opts.indef_allowed {
                        return Err(CborError::IndefiniteLengthNotAllowed(t));
                    }
                    stack.push(Frame::Chunks(t));
                    continue;
                }
                if val > i64::MAX as u64 {
                    return Err(CborError::StringLengthOverflow { t, len: val });
                }
                if ((data.len() - *off) as u64) < val {
                    return Err(CborError::UnexpectedEof);
                }
                *off += val as usize;
                if complete_item(&mut stack, opts)? {
                    return Ok(());
                }
            }
            CborType::Array | CborType::Map => {
                depth += 1;
                if depth > opts.max_nested {
                    return Err(CborError::MaxNestedLevel(opts.max_nested));
                }
                if indef {
                    if !opts.indef_allowed {
                        return Err(CborError::IndefiniteLengthNotAllowed(t));
                    }
                    stack.push(Frame::Indefinite {
                        map: t == CborType::Map,
                        count: 0,
                        depth,
                    });
                    continue;
                }
                if val > i64::MAX as u64 {
                    return Err(CborError::ContainerLengthOverflow { t, len: val });
                }
                if t == CborType::Array {
                    if val > opts.max_array {
                        return Err(CborError::MaxArrayElements(opts.max_array as usize));
                    }
                } else if val > opts.max_map {
                    return Err(CborError::MaxMapPairs(opts.max_map as usize));
                }
                let items = if t == CborType::Map { val * 2 } else { val };
                if items == 0 {
                    if complete_item(&mut stack, opts)? {
                        return Ok(());
                    }
                } else {
                    stack.push(Frame::Definite {
                        remaining: items,
                        depth,
                    });
                }
            }
            CborType::Tag => {
                // Scan the chain of nested tag numbers without recursion; the
                // first tag does not count toward nesting, each further one
                // does (fxamacker quirk).
                let mut tag_num = val;
                loop {
                    if *off == data.len() {
                        // A tag number must be followed by tag content.
                        return Err(CborError::UnexpectedEof);
                    }
                    if opts.check_builtin_tags {
                        valid_builtin_tag(tag_num, data[*off])?;
                    }
                    if data[*off] >> 5 != 6 {
                        break;
                    }
                    let (_, _, next_num) = wf_head(data, off)?;
                    tag_num = next_num;
                    depth += 1;
                    if depth > opts.max_nested {
                        return Err(CborError::MaxNestedLevel(opts.max_nested));
                    }
                }
                tag_content_depth = Some(depth);
            }
        }
    }
}

/// Bookkeeping after one complete item: unwind finished definite containers
/// and count elements of indefinite ones. Returns `true` when the top-level
/// item is complete.
fn complete_item(stack: &mut Vec<Frame>, opts: &WfOpts) -> Result<bool, CborError> {
    loop {
        match stack.last_mut() {
            None => return Ok(true),
            Some(Frame::Definite { remaining, .. }) => {
                *remaining -= 1;
                if *remaining == 0 {
                    stack.pop();
                    continue; // the container itself is a completed item
                }
                return Ok(false);
            }
            Some(Frame::Indefinite { map, count, .. }) => {
                *count += 1;
                if *map {
                    if *count % 2 == 0 && *count / 2 > opts.max_map {
                        return Err(CborError::MaxMapPairs(opts.max_map as usize));
                    }
                } else if *count > opts.max_array {
                    return Err(CborError::MaxArrayElements(opts.max_array as usize));
                }
                return Ok(false);
            }
            Some(Frame::Chunks(_)) => return Ok(false), // one chunk done
        }
    }
}

/// Port of fxamacker `wellformedHead` (minus the NaN/Inf checks, which are
/// no-ops under the default and core-deterministic modes).
fn wf_head(data: &[u8], off: &mut usize) -> Result<(CborType, u8, u64), CborError> {
    if *off == data.len() {
        return Err(CborError::UnexpectedEof);
    }
    let b = data[*off];
    *off += 1;
    let t = CborType::from_initial_byte(b);
    let ai = b & 0x1f;
    match ai {
        0..=23 => Ok((t, ai, u64::from(ai))),
        24 => {
            if data.len() - *off < 1 {
                return Err(CborError::UnexpectedEof);
            }
            let val = u64::from(data[*off]);
            *off += 1;
            if t == CborType::Primitives && val < 32 {
                return Err(CborError::InvalidSimpleValue(val as u8));
            }
            Ok((t, ai, val))
        }
        25 => {
            if data.len() - *off < 2 {
                return Err(CborError::UnexpectedEof);
            }
            let val = u64::from(u16::from_be_bytes([data[*off], data[*off + 1]]));
            *off += 2;
            Ok((t, ai, val))
        }
        26 => {
            if data.len() - *off < 4 {
                return Err(CborError::UnexpectedEof);
            }
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&data[*off..*off + 4]);
            *off += 4;
            Ok((t, ai, u64::from(u32::from_be_bytes(buf))))
        }
        27 => {
            if data.len() - *off < 8 {
                return Err(CborError::UnexpectedEof);
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&data[*off..*off + 8]);
            *off += 8;
            Ok((t, ai, u64::from_be_bytes(buf)))
        }
        31 => match t {
            CborType::PositiveInt | CborType::NegativeInt | CborType::Tag => {
                Err(CborError::InvalidAdditionalInformation { ai, t })
            }
            CborType::Primitives => Err(CborError::UnexpectedBreak),
            _ => Ok((t, ai, u64::from(ai))),
        },
        // 28, 29, 30
        _ => Err(CborError::InvalidAdditionalInformation { ai, t }),
    }
}

/// Port of fxamacker `validBuiltinTag`: supported built-in tag numbers must
/// be followed by the expected content type.
fn valid_builtin_tag(tag_num: u64, content_head: u8) -> Result<(), CborError> {
    let t = CborType::from_initial_byte(content_head);
    match tag_num {
        0 => {
            if t != CborType::TextString {
                return Err(CborError::InadmissibleTagContent {
                    tag: "0",
                    expected: "text string",
                    got: t,
                });
            }
            Ok(())
        }
        1 => {
            if t != CborType::PositiveInt
                && t != CborType::NegativeInt
                && !(0xf9..=0xfb).contains(&content_head)
            {
                return Err(CborError::InadmissibleTagContent {
                    tag: "1",
                    expected: "integer or floating-point number",
                    got: t,
                });
            }
            Ok(())
        }
        2 | 3 => {
            if t != CborType::ByteString {
                return Err(CborError::InadmissibleTagContent {
                    tag: "2 or 3",
                    expected: "byte string",
                    got: t,
                });
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Typed decode phase (assumes the input passed `wellformed`).
// ---------------------------------------------------------------------------

/// Unmarshals a CBOR array of byte strings (Go `[][]byte`), for FileNode.
pub(super) fn unmarshal_byte_slices(data: &[u8]) -> Result<Vec<Vec<u8>>, CborError> {
    wellformed(data, &DECODE_WF)?;
    let mut d = Dec::new(data);
    let (v, err) = d.parse_to_byte_slices();
    match err {
        Some(e) => Err(e),
        None => Ok(v),
    }
}

/// Unmarshals a CBOR array of entry maps (Go `[]fstree.Entry`), for DirLeaf.
pub(super) fn unmarshal_entries(data: &[u8]) -> Result<Vec<Entry>, CborError> {
    wellformed(data, &DECODE_WF)?;
    let mut d = Dec::new(data);
    let (v, err) = d.parse_to_entries();
    match err {
        Some(e) => Err(e),
        None => Ok(v),
    }
}

/// Unmarshals a CBOR array of `[sepName, childKey]` pairs (Go
/// `[]fstree.DirPair`), for DirNode.
pub(super) fn unmarshal_pairs(data: &[u8]) -> Result<Vec<DirPair>, CborError> {
    wellformed(data, &DECODE_WF)?;
    let mut d = Dec::new(data);
    let (v, err) = d.parse_to_pairs();
    match err {
        Some(e) => Err(e),
        None => Ok(v),
    }
}

/// Unmarshals a Commit body (Go `commit.wireCommit`, a `keyasint` struct
/// holding two nested `commit.wireIdentity` structs). The caller — package
/// `commit` — converts, validates and canonical-checks the result.
pub(crate) fn unmarshal_commit(data: &[u8]) -> Result<WireCommit, CborError> {
    wellformed(data, &DECODE_WF)?;
    let mut d = Dec::new(data);
    let (v, err) = d.parse_to_wire_commit();
    match err {
        Some(e) => Err(e),
        None => Ok(v),
    }
}

/// Go struct-field diagnostic names for `commit.wireCommit` keys 0–6.
const WIRE_COMMIT_FIELDS: [&str; 7] = [
    "commit.wireCommit.0",
    "commit.wireCommit.1",
    "commit.wireCommit.2",
    "commit.wireCommit.3",
    "commit.wireCommit.4",
    "commit.wireCommit.5",
    "commit.wireCommit.6",
];

/// Go struct-field diagnostic names for `commit.wireIdentity` keys 0–3.
const WIRE_IDENTITY_FIELDS: [&str; 4] = [
    "commit.wireIdentity.0",
    "commit.wireIdentity.1",
    "commit.wireIdentity.2",
    "commit.wireIdentity.3",
];

/// Go struct-field diagnostic names for `Entry` keys 0–9.
const ENTRY_FIELDS: [&str; 10] = [
    "fstree.Entry.0",
    "fstree.Entry.1",
    "fstree.Entry.2",
    "fstree.Entry.3",
    "fstree.Entry.4",
    "fstree.Entry.5",
    "fstree.Entry.6",
    "fstree.Entry.7",
    "fstree.Entry.8",
    "fstree.Entry.9",
];

fn type_err(t: CborType, go_type: &'static str) -> CborError {
    CborError::UnmarshalType {
        t,
        go_type,
        field: None,
        msg: None,
    }
}

fn type_err_msg(t: CborType, go_type: &'static str, msg: String) -> CborError {
    CborError::UnmarshalType {
        t,
        go_type,
        field: None,
        msg: Some(msg),
    }
}

/// Sets the struct-field context on an `UnmarshalType` error, like
/// `decodeToStructField` / `parseArrayToStruct` do. Other error kinds pass
/// through unchanged.
fn wrap_field(err: CborError, fld: &'static str) -> CborError {
    match err {
        CborError::UnmarshalType {
            t, go_type, msg, ..
        } => CborError::UnmarshalType {
            t,
            go_type,
            field: Some(fld),
            msg,
        },
        e => e,
    }
}

/// `fillPositiveInt` into an unsigned Go type of maximum value `max`.
fn fill_uint(t: CborType, val: u64, go_type: &'static str, max: u64) -> (u64, Option<CborError>) {
    if val > max {
        return (
            0,
            Some(type_err_msg(
                t,
                go_type,
                format!("{val} overflows {go_type}"),
            )),
        );
    }
    (val, None)
}

/// The decimal rendering of the big-endian unsigned integer `b` (Go
/// `big.Int.SetBytes(b).String()`); with `negate`, the rendering of
/// `-(b + 1)` (bignum tag 3 semantics).
fn bignum_dec(b: &[u8], negate: bool) -> String {
    let mut mag: Vec<u8> = b.to_vec();
    if negate {
        // mag += 1, big-endian.
        let mut carry = true;
        for byte in mag.iter_mut().rev() {
            if !carry {
                break;
            }
            let (nb, c) = byte.overflowing_add(1);
            *byte = nb;
            carry = c;
        }
        if carry {
            mag.insert(0, 1);
        }
    }
    // Repeated division by 10.
    let mut digits = Vec::new();
    let mut start = 0usize;
    loop {
        while start < mag.len() && mag[start] == 0 {
            start += 1;
        }
        if start == mag.len() {
            break;
        }
        let mut rem = 0u32;
        for byte in &mut mag[start..] {
            let cur = rem * 256 + u32::from(*byte);
            *byte = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    if negate {
        digits.push(b'-');
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

/// The value of the big-endian bignum `b` if it fits a u64 (Go
/// `big.Int.IsUint64`).
fn bignum_u64(b: &[u8]) -> Option<u64> {
    let sig = match b.iter().position(|&x| x != 0) {
        Some(p) => &b[p..],
        None => return Some(0),
    };
    if sig.len() > 8 {
        return None;
    }
    let mut v = 0u64;
    for &byte in sig {
        v = v << 8 | u64::from(byte);
    }
    Some(v)
}

/// The typed-decode cursor (fxamacker `decoder`); assumes well-formed input,
/// so the bounds clamps in the low-level readers are unreachable and exist
/// only to uphold the crate's no-panic rule against latent bugs.
pub(super) struct Dec<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> Dec<'a> {
    pub(super) fn new(data: &'a [u8]) -> Dec<'a> {
        Dec { data, off: 0 }
    }

    fn next_initial_byte(&self) -> u8 {
        if self.off < self.data.len() {
            self.data[self.off]
        } else {
            debug_assert!(false, "decode ran past well-formed input");
            0xf6 // benign: CBOR null
        }
    }

    fn next_type(&self) -> CborType {
        CborType::from_initial_byte(self.next_initial_byte())
    }

    /// Reads one head without validation (`getHead`); input is well-formed.
    fn get_head(&mut self) -> (CborType, u8, u64) {
        let b = self.next_initial_byte();
        self.off = (self.off + 1).min(self.data.len());
        let t = CborType::from_initial_byte(b);
        let ai = b & 0x1f;
        let n = match ai {
            24 => 1,
            25 => 2,
            26 => 4,
            27 => 8,
            _ => return (t, ai, u64::from(ai)),
        };
        let mut val = 0u64;
        for _ in 0..n {
            val = val << 8 | u64::from(self.next_initial_byte());
            self.off = (self.off + 1).min(self.data.len());
        }
        (t, ai, val)
    }

    fn read(&mut self, n: u64) -> &'a [u8] {
        let n = (n as usize).min(self.data.len() - self.off);
        let s = &self.data[self.off..self.off + n];
        self.off += n;
        s
    }

    fn found_break(&mut self) -> bool {
        if self.off < self.data.len() && self.data[self.off] == 0xff {
            self.off += 1;
            true
        } else {
            false
        }
    }

    /// Skips exactly one item (`decoder.skip`).
    fn skip(&mut self) {
        let (t, ai, val) = self.get_head();
        if ai == 31 {
            if matches!(
                t,
                CborType::ByteString | CborType::TextString | CborType::Array | CborType::Map
            ) {
                while !self.found_break() {
                    if self.off >= self.data.len() {
                        return; // unreachable on well-formed input
                    }
                    self.skip();
                }
            }
            return;
        }
        match t {
            CborType::ByteString | CborType::TextString => {
                self.read(val);
            }
            CborType::Array => {
                for _ in 0..val {
                    self.skip();
                }
            }
            CborType::Map => {
                for _ in 0..val.saturating_mul(2) {
                    self.skip();
                }
            }
            CborType::Tag => self.skip(),
            _ => {}
        }
    }

    /// Counts the items of an indefinite container up to its break without
    /// consuming them (`numOfItemsUntilBreak`).
    fn num_items_until_break(&mut self) -> u64 {
        let saved = self.off;
        let mut i = 0u64;
        while !self.found_break() {
            if self.off >= self.data.len() {
                break; // unreachable on well-formed input
            }
            self.skip();
            i += 1;
        }
        self.off = saved;
        i
    }

    /// Reads a byte string, concatenating indefinite chunks
    /// (`parseByteString`).
    fn parse_byte_string(&mut self) -> Vec<u8> {
        let (_, ai, val) = self.get_head();
        if ai != 31 {
            return self.read(val).to_vec();
        }
        let mut b = Vec::new();
        while !self.found_break() {
            if self.off >= self.data.len() {
                break; // unreachable on well-formed input
            }
            let (_, _, n) = self.get_head();
            b.extend_from_slice(self.read(n));
        }
        b
    }

    /// Reads a text string, rejecting invalid UTF-8 (`parseTextString` with
    /// the default `UTF8RejectInvalid`). On an invalid indefinite chunk the
    /// remaining chunks are skipped, exactly like Go.
    fn parse_text_string(&mut self) -> Result<Vec<u8>, CborError> {
        let (_, ai, val) = self.get_head();
        if ai != 31 {
            let b = self.read(val);
            if std::str::from_utf8(b).is_err() {
                return Err(CborError::InvalidUtf8);
            }
            return Ok(b.to_vec());
        }
        let mut b = Vec::new();
        while !self.found_break() {
            if self.off >= self.data.len() {
                break; // unreachable on well-formed input
            }
            let (_, _, n) = self.get_head();
            let x = self.read(n);
            if std::str::from_utf8(x).is_err() {
                while !self.found_break() {
                    if self.off >= self.data.len() {
                        break;
                    }
                    self.skip();
                }
                return Err(CborError::InvalidUtf8);
            }
            b.extend_from_slice(x);
        }
        Ok(b)
    }

    /// The `parseToValue` prologue shared by every target: strip
    /// self-described-CBOR tags (55799), then validate built-in tag content
    /// across the remaining tag chain. On failure the whole item is consumed
    /// (Go does `d.skip()` from mid-chain).
    fn prologue(&mut self) -> Result<(), CborError> {
        loop {
            if self.next_type() != CborType::Tag {
                break;
            }
            let save = self.off;
            let (_, _, num) = self.get_head();
            if num != 55799 {
                self.off = save;
                break;
            }
        }
        let save = self.off;
        while self.next_type() == CborType::Tag {
            let (_, _, num) = self.get_head();
            if let Err(e) = valid_builtin_tag(num, self.next_initial_byte()) {
                self.skip();
                return Err(e);
            }
        }
        self.off = save;
        Ok(())
    }

    /// Decodes into an unsigned Go integer (`uint64` or, for byte-slice
    /// elements, `uint8`). Returns the value (0 on error) and the first
    /// error; always consumes exactly one item.
    fn parse_to_uint(&mut self, go_type: &'static str, max: u64) -> (u64, Option<CborError>) {
        if let Err(e) = self.prologue() {
            return (0, Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::PositiveInt => {
                let (_, _, val) = self.get_head();
                fill_uint(t, val, go_type, max)
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        0,
                        Some(type_err_msg(
                            t,
                            go_type,
                            format!("{dec} overflows Go's int64"),
                        )),
                    )
                } else {
                    (0, Some(type_err(t, go_type)))
                }
            }
            CborType::ByteString => {
                self.parse_byte_string();
                (0, Some(type_err(t, go_type)))
            }
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (0, Some(e)),
                Ok(_) => (0, Some(type_err(t, go_type))),
            },
            CborType::Primitives => {
                let (_, ai, val) = self.get_head();
                match ai {
                    20 | 21 | 25 | 26 | 27 => (0, Some(type_err(t, go_type))),
                    22 | 23 => (0, None), // null/undefined: no-op
                    _ => fill_uint(t, val, go_type, max),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    2 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(val) => fill_uint(CborType::Tag, val, go_type, max),
                            None => {
                                let dec = bignum_dec(&b, false);
                                (
                                    0,
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        go_type,
                                        format!("{dec} overflows {go_type}"),
                                    )),
                                )
                            }
                        }
                    }
                    3 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(x) if x <= i64::MAX as u64 => {
                                (0, Some(type_err(CborType::Tag, go_type)))
                            }
                            _ => {
                                let dec = bignum_dec(&b, true);
                                (
                                    0,
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        go_type,
                                        format!("{dec} overflows {go_type}"),
                                    )),
                                )
                            }
                        }
                    }
                    _ => self.parse_to_uint(go_type, max),
                }
            }
            CborType::Array | CborType::Map => {
                self.skip();
                (0, Some(type_err(t, go_type)))
            }
        }
    }

    /// Decodes into Go `int64` (mtime).
    fn parse_to_i64(&mut self) -> (i64, Option<CborError>) {
        const GO: &str = "int64";
        if let Err(e) = self.prologue() {
            return (0, Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::PositiveInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    (
                        0,
                        Some(type_err_msg(t, GO, format!("{val} overflows int64"))),
                    )
                } else {
                    (val as i64, None)
                }
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        0,
                        Some(type_err_msg(t, GO, format!("{dec} overflows Go's int64"))),
                    )
                } else {
                    (!(val as i64), None)
                }
            }
            CborType::ByteString => {
                self.parse_byte_string();
                (0, Some(type_err(t, GO)))
            }
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (0, Some(e)),
                Ok(_) => (0, Some(type_err(t, GO))),
            },
            CborType::Primitives => {
                let (_, ai, val) = self.get_head();
                match ai {
                    20 | 21 | 25 | 26 | 27 => (0, Some(type_err(t, GO))),
                    22 | 23 => (0, None),
                    _ => (val as i64, None), // simple values are ≤ 255
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    2 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(val) if val <= i64::MAX as u64 => (val as i64, None),
                            Some(val) => (
                                0,
                                Some(type_err_msg(
                                    CborType::Tag,
                                    GO,
                                    format!("{val} overflows int64"),
                                )),
                            ),
                            None => {
                                let dec = bignum_dec(&b, false);
                                (
                                    0,
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        GO,
                                        format!("{dec} overflows int64"),
                                    )),
                                )
                            }
                        }
                    }
                    3 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(x) if x <= i64::MAX as u64 => ((!(x as i64)), None),
                            _ => {
                                let dec = bignum_dec(&b, true);
                                (
                                    0,
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        GO,
                                        format!("{dec} overflows int64"),
                                    )),
                                )
                            }
                        }
                    }
                    _ => self.parse_to_i64(),
                }
            }
            CborType::Array | CborType::Map => {
                self.skip();
                (0, Some(type_err(t, GO)))
            }
        }
    }

    /// Decodes into Go `[]byte` (`go_type` names the destination in
    /// diagnostics, e.g. `[]uint8`). Arrays decode element-wise into bytes,
    /// like Go reflection does for `[]uint8`.
    fn parse_to_bytes(&mut self, go_type: &'static str) -> (Vec<u8>, Option<CborError>) {
        if let Err(e) = self.prologue() {
            return (Vec::new(), Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::ByteString => (self.parse_byte_string(), None),
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (Vec::new(), Some(e)),
                Ok(_) => (Vec::new(), Some(type_err(t, go_type))),
            },
            CborType::PositiveInt => {
                self.get_head();
                (Vec::new(), Some(type_err(t, go_type)))
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        Vec::new(),
                        Some(type_err_msg(
                            t,
                            go_type,
                            format!("{dec} overflows Go's int64"),
                        )),
                    )
                } else {
                    (Vec::new(), Some(type_err(t, go_type)))
                }
            }
            CborType::Primitives => {
                let (_, ai, _) = self.get_head();
                match ai {
                    22 | 23 => (Vec::new(), None), // null/undefined → nil slice
                    _ => (Vec::new(), Some(type_err(t, go_type))),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    // Bignum content bytes fill the byte slice directly.
                    2 | 3 => (self.parse_byte_string(), None),
                    _ => self.parse_to_bytes(go_type),
                }
            }
            CborType::Array => {
                let (_, ai, val) = self.get_head();
                let indef = ai == 31;
                let count = if indef {
                    self.num_items_until_break()
                } else {
                    val
                };
                let mut out = vec![0u8; count as usize];
                let mut first = None;
                let mut i = 0usize;
                loop {
                    if indef {
                        if self.found_break() {
                            break;
                        }
                    } else if i as u64 >= count {
                        break;
                    }
                    let (v, e) = self.parse_to_uint("uint8", 255);
                    if let Some(slot) = out.get_mut(i) {
                        *slot = v as u8;
                    }
                    if first.is_none() {
                        first = e;
                    }
                    i += 1;
                }
                (out, first)
            }
            CborType::Map => {
                self.skip();
                (Vec::new(), Some(type_err(t, go_type)))
            }
        }
    }

    /// Decodes into Go `[]uint64` (rdev).
    fn parse_to_u64s(&mut self) -> (Vec<u64>, Option<CborError>) {
        const GO: &str = "[]uint64";
        if let Err(e) = self.prologue() {
            return (Vec::new(), Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::Array => {
                let (_, ai, val) = self.get_head();
                let indef = ai == 31;
                let count = if indef {
                    self.num_items_until_break()
                } else {
                    val
                };
                let mut out = vec![0u64; count as usize];
                let mut first = None;
                let mut i = 0usize;
                loop {
                    if indef {
                        if self.found_break() {
                            break;
                        }
                    } else if i as u64 >= count {
                        break;
                    }
                    let (v, e) = self.parse_to_uint("uint64", u64::MAX);
                    if let Some(slot) = out.get_mut(i) {
                        *slot = v;
                    }
                    if first.is_none() {
                        first = e;
                    }
                    i += 1;
                }
                (out, first)
            }
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (Vec::new(), Some(e)),
                Ok(_) => (Vec::new(), Some(type_err(t, GO))),
            },
            CborType::ByteString => {
                self.parse_byte_string();
                (Vec::new(), Some(type_err(t, GO)))
            }
            CborType::PositiveInt => {
                self.get_head();
                (Vec::new(), Some(type_err(t, GO)))
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        Vec::new(),
                        Some(type_err_msg(t, GO, format!("{dec} overflows Go's int64"))),
                    )
                } else {
                    (Vec::new(), Some(type_err(t, GO)))
                }
            }
            CborType::Primitives => {
                let (_, ai, _) = self.get_head();
                match ai {
                    22 | 23 => (Vec::new(), None),
                    _ => (Vec::new(), Some(type_err(t, GO))),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    // fillByteString into []uint64: element type is not uint8.
                    2 | 3 => {
                        self.parse_byte_string();
                        (Vec::new(), Some(type_err(CborType::Tag, GO)))
                    }
                    _ => self.parse_to_u64s(),
                }
            }
            CborType::Map => {
                self.skip();
                (Vec::new(), Some(type_err(t, GO)))
            }
        }
    }

    /// Decodes into Go `cbor.RawMessage`: captures the raw bytes of one item
    /// verbatim (after stripping self-described-CBOR tags and validating
    /// built-in tags, per the `parseToValue` prologue).
    fn parse_to_raw(&mut self) -> (Vec<u8>, Option<CborError>) {
        if let Err(e) = self.prologue() {
            return (Vec::new(), Some(e));
        }
        let start = self.off;
        self.skip();
        (self.data[start..self.off].to_vec(), None)
    }

    /// Decodes into an `Entry` (Go map-to-struct with `keyasint` fields).
    fn parse_to_entry(&mut self) -> (Entry, Option<CborError>) {
        const GO: &str = "fstree.Entry";
        if let Err(e) = self.prologue() {
            return (Entry::default(), Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::Map => self.parse_map_to_entry(),
            CborType::Array => {
                // Struct without the toarray option.
                self.skip();
                (
                    Entry::default(),
                    Some(type_err_msg(
                        t,
                        GO,
                        "cannot decode CBOR array to struct without toarray option".to_owned(),
                    )),
                )
            }
            CborType::PositiveInt => {
                self.get_head();
                (Entry::default(), Some(type_err(t, GO)))
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        Entry::default(),
                        Some(type_err_msg(t, GO, format!("{dec} overflows Go's int64"))),
                    )
                } else {
                    (Entry::default(), Some(type_err(t, GO)))
                }
            }
            CborType::ByteString => {
                self.parse_byte_string();
                (Entry::default(), Some(type_err(t, GO)))
            }
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (Entry::default(), Some(e)),
                Ok(_) => (Entry::default(), Some(type_err(t, GO))),
            },
            CborType::Primitives => {
                let (_, ai, _) = self.get_head();
                match ai {
                    22 | 23 => (Entry::default(), None), // zero value
                    _ => (Entry::default(), Some(type_err(t, GO))),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    2 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(_) => (Entry::default(), Some(type_err(CborType::Tag, GO))),
                            None => {
                                let dec = bignum_dec(&b, false);
                                (
                                    Entry::default(),
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        GO,
                                        format!("{dec} overflows {GO}"),
                                    )),
                                )
                            }
                        }
                    }
                    3 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(x) if x <= i64::MAX as u64 => {
                                (Entry::default(), Some(type_err(CborType::Tag, GO)))
                            }
                            _ => {
                                let dec = bignum_dec(&b, true);
                                (
                                    Entry::default(),
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        GO,
                                        format!("{dec} overflows {GO}"),
                                    )),
                                )
                            }
                        }
                    }
                    _ => self.parse_to_entry(),
                }
            }
        }
    }

    /// The `parseMapToStruct` mirror for `Entry`: integer keys 0–9 match
    /// fields, first occurrence wins, unknown keys are skipped, text keys
    /// never match, other key types are diagnosed but tolerated.
    fn parse_map_to_entry(&mut self) -> (Entry, Option<CborError>) {
        let (_, ai, val) = self.get_head();
        let indef = ai == 31;
        let mut e = Entry::default();
        let mut found = [false; 10];
        let mut first: Option<CborError> = None;
        let record = |first: &mut Option<CborError>, err: Option<CborError>| {
            if first.is_none() {
                *first = err;
            }
        };
        let mut i = 0u64;
        loop {
            if indef {
                if self.found_break() {
                    break;
                }
            } else if i >= val {
                break;
            }
            let kt = self.next_type();
            match kt {
                CborType::PositiveInt => {
                    let (_, _, k) = self.get_head();
                    if k > i64::MAX as u64 {
                        record(
                            &mut first,
                            Some(type_err_msg(
                                kt,
                                "int64",
                                format!("{k} overflows Go's int64"),
                            )),
                        );
                        self.skip();
                    } else if k <= 9 && !found[k as usize] {
                        found[k as usize] = true;
                        let fld = ENTRY_FIELDS[k as usize];
                        let err = match k {
                            0 => {
                                let (v, err) = self.parse_to_bytes("[]uint8");
                                e.name = v;
                                err
                            }
                            1 => {
                                let (v, err) = self.parse_to_uint("uint64", u64::MAX);
                                e.mode = v;
                                err
                            }
                            2 => {
                                let (v, err) = self.parse_to_uint("uint64", u64::MAX);
                                e.uid = v;
                                err
                            }
                            3 => {
                                let (v, err) = self.parse_to_uint("uint64", u64::MAX);
                                e.gid = v;
                                err
                            }
                            4 => {
                                let (v, err) = self.parse_to_i64();
                                e.mtime = v;
                                err
                            }
                            5 => {
                                let (v, err) = self.parse_to_bytes("[]uint8");
                                e.content_key = v;
                                err
                            }
                            6 => {
                                let (v, err) = self.parse_to_bytes("[]uint8");
                                e.link_target = v;
                                err
                            }
                            7 => {
                                let (v, err) = self.parse_to_u64s();
                                e.rdev = v;
                                err
                            }
                            8 => {
                                let (v, err) = self.parse_to_raw();
                                e.xattrs_in = v;
                                err
                            }
                            _ => {
                                let (v, err) = self.parse_to_bytes("[]uint8");
                                e.xattrs_key = v;
                                err
                            }
                        };
                        record(&mut first, err.map(|er| wrap_field(er, fld)));
                    } else {
                        // Unknown positive key, or a duplicate: skip value.
                        self.skip();
                    }
                }
                CborType::NegativeInt => {
                    let (_, _, k) = self.get_head();
                    if k > i64::MAX as u64 {
                        record(
                            &mut first,
                            Some(type_err_msg(
                                kt,
                                "int64",
                                format!("-1-{k} overflows Go's int64"),
                            )),
                        );
                    }
                    // Negative keys never match fields 0–9: skip value.
                    self.skip();
                }
                CborType::TextString => {
                    match self.parse_text_string() {
                        Err(err) => record(&mut first, Some(err)),
                        Ok(_) => {
                            // keyasint fields are not matchable by name.
                        }
                    }
                    self.skip(); // value
                }
                _ => {
                    record(
                        &mut first,
                        Some(type_err_msg(
                            kt,
                            "string",
                            format!(
                                "map key is of type {kt} and cannot be used to match struct field name"
                            ),
                        )),
                    );
                    self.skip(); // key
                    self.skip(); // value
                }
            }
            i += 1;
        }
        (e, first)
    }

    /// Decodes into a `DirPair` (Go `toarray` struct).
    fn parse_to_pair(&mut self) -> (DirPair, Option<CborError>) {
        const GO: &str = "fstree.DirPair";
        if let Err(e) = self.prologue() {
            return (DirPair::default(), Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::Array => {
                let start = self.off;
                let (_, ai, val) = self.get_head();
                let indef = ai == 31;
                let count = if indef {
                    self.num_items_until_break()
                } else {
                    val
                };
                if count != 2 {
                    self.off = start;
                    self.skip();
                    return (
                        DirPair::default(),
                        Some(type_err_msg(
                            CborType::Array,
                            GO,
                            "cannot decode CBOR array to struct with different number of elements"
                                .to_owned(),
                        )),
                    );
                }
                let mut first = None;
                let (sep, e1) = self.parse_to_bytes("[]uint8");
                if first.is_none() {
                    first = e1.map(|e| wrap_field(e, "fstree.DirPair.SepName"));
                }
                let (child, e2) = self.parse_to_bytes("[]uint8");
                if first.is_none() {
                    first = e2.map(|e| wrap_field(e, "fstree.DirPair.ChildKey"));
                }
                if indef {
                    self.found_break();
                }
                (
                    DirPair {
                        sep_name: sep,
                        child_key: child,
                    },
                    first,
                )
            }
            CborType::Map => {
                self.skip();
                (
                    DirPair::default(),
                    Some(type_err_msg(
                        t,
                        GO,
                        "cannot decode CBOR map to struct with toarray option".to_owned(),
                    )),
                )
            }
            CborType::PositiveInt => {
                self.get_head();
                (DirPair::default(), Some(type_err(t, GO)))
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        DirPair::default(),
                        Some(type_err_msg(t, GO, format!("{dec} overflows Go's int64"))),
                    )
                } else {
                    (DirPair::default(), Some(type_err(t, GO)))
                }
            }
            CborType::ByteString => {
                self.parse_byte_string();
                (DirPair::default(), Some(type_err(t, GO)))
            }
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (DirPair::default(), Some(e)),
                Ok(_) => (DirPair::default(), Some(type_err(t, GO))),
            },
            CborType::Primitives => {
                let (_, ai, _) = self.get_head();
                match ai {
                    22 | 23 => (DirPair::default(), None),
                    _ => (DirPair::default(), Some(type_err(t, GO))),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    2 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(_) => (DirPair::default(), Some(type_err(CborType::Tag, GO))),
                            None => {
                                let dec = bignum_dec(&b, false);
                                (
                                    DirPair::default(),
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        GO,
                                        format!("{dec} overflows {GO}"),
                                    )),
                                )
                            }
                        }
                    }
                    3 => {
                        let b = self.parse_byte_string();
                        match bignum_u64(&b) {
                            Some(x) if x <= i64::MAX as u64 => {
                                (DirPair::default(), Some(type_err(CborType::Tag, GO)))
                            }
                            _ => {
                                let dec = bignum_dec(&b, true);
                                (
                                    DirPair::default(),
                                    Some(type_err_msg(
                                        CborType::Tag,
                                        GO,
                                        format!("{dec} overflows {GO}"),
                                    )),
                                )
                            }
                        }
                    }
                    _ => self.parse_to_pair(),
                }
            }
        }
    }

    /// Shared slice-target dispatch (`parseToValue` for a Go slice type that
    /// is not `[]byte`): handles everything but the element loop.
    fn parse_to_slice_of<T: Default>(
        &mut self,
        go_type: &'static str,
        mut elem: impl FnMut(&mut Self) -> (T, Option<CborError>),
    ) -> (Vec<T>, Option<CborError>) {
        if let Err(e) = self.prologue() {
            return (Vec::new(), Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::Array => {
                let (_, ai, val) = self.get_head();
                let indef = ai == 31;
                let count = if indef {
                    self.num_items_until_break()
                } else {
                    val
                };
                let mut out: Vec<T> = Vec::with_capacity((count as usize).min(4096));
                let mut first = None;
                let mut i = 0u64;
                loop {
                    if indef {
                        if self.found_break() {
                            break;
                        }
                    } else if i >= count {
                        break;
                    }
                    let (v, e) = elem(self);
                    out.push(v);
                    if first.is_none() {
                        first = e;
                    }
                    i += 1;
                }
                (out, first)
            }
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (Vec::new(), Some(e)),
                Ok(_) => (Vec::new(), Some(type_err(t, go_type))),
            },
            CborType::ByteString => {
                self.parse_byte_string();
                (Vec::new(), Some(type_err(t, go_type)))
            }
            CborType::PositiveInt => {
                self.get_head();
                (Vec::new(), Some(type_err(t, go_type)))
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        Vec::new(),
                        Some(type_err_msg(
                            t,
                            go_type,
                            format!("{dec} overflows Go's int64"),
                        )),
                    )
                } else {
                    (Vec::new(), Some(type_err(t, go_type)))
                }
            }
            CborType::Primitives => {
                let (_, ai, _) = self.get_head();
                match ai {
                    22 | 23 => (Vec::new(), None),
                    _ => (Vec::new(), Some(type_err(t, go_type))),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    // fillByteString into a non-u8-element slice.
                    2 | 3 => {
                        self.parse_byte_string();
                        (Vec::new(), Some(type_err(CborType::Tag, go_type)))
                    }
                    _ => self.parse_to_slice_of(go_type, elem),
                }
            }
            CborType::Map => {
                self.skip();
                (Vec::new(), Some(type_err(t, go_type)))
            }
        }
    }

    /// Decodes into Go `string` (default `ByteStringToStringForbidden`): only
    /// a text string fills it; null/undefined are a no-op; bignum tags take
    /// the integer paths and end as type errors; other tags unwrap.
    fn parse_to_string(&mut self) -> (String, Option<CborError>) {
        const GO: &str = "string";
        if let Err(e) = self.prologue() {
            return (String::new(), Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::TextString => match self.parse_text_string() {
                // parse_text_string has already rejected invalid UTF-8.
                Ok(b) => (String::from_utf8(b).unwrap_or_default(), None),
                Err(e) => (String::new(), Some(e)),
            },
            CborType::ByteString => {
                self.parse_byte_string();
                (String::new(), Some(type_err(t, GO)))
            }
            CborType::PositiveInt => {
                self.get_head();
                (String::new(), Some(type_err(t, GO)))
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        String::new(),
                        Some(type_err_msg(t, GO, format!("{dec} overflows Go's int64"))),
                    )
                } else {
                    (String::new(), Some(type_err(t, GO)))
                }
            }
            CborType::Primitives => {
                let (_, ai, _) = self.get_head();
                match ai {
                    22 | 23 => (String::new(), None), // null/undefined: no-op
                    _ => (String::new(), Some(type_err(t, GO))),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    2 | 3 => {
                        let b = self.parse_byte_string();
                        let fits = match bignum_u64(&b) {
                            Some(x) => num == 2 || x <= i64::MAX as u64,
                            None => false,
                        };
                        if fits {
                            (String::new(), Some(type_err(CborType::Tag, GO)))
                        } else {
                            let dec = bignum_dec(&b, num == 3);
                            (
                                String::new(),
                                Some(type_err_msg(
                                    CborType::Tag,
                                    GO,
                                    format!("{dec} overflows {GO}"),
                                )),
                            )
                        }
                    }
                    _ => self.parse_to_string(),
                }
            }
            CborType::Array | CborType::Map => {
                self.skip();
                (String::new(), Some(type_err(t, GO)))
            }
        }
    }

    /// `parseToValue` into a Go struct without the `toarray` option: a map
    /// goes to `map` (a `parse_map_to_struct` driver), null/undefined leave
    /// the zero value, everything else is a type error against `go_type`.
    /// The same arms as [`Dec::parse_to_entry`], which predates this helper.
    fn parse_to_struct<T: Default>(
        &mut self,
        go_type: &'static str,
        map: impl FnOnce(&mut Self) -> (T, Option<CborError>),
    ) -> (T, Option<CborError>) {
        if let Err(e) = self.prologue() {
            return (T::default(), Some(e));
        }
        let t = self.next_type();
        match t {
            CborType::Map => map(self),
            CborType::Array => {
                self.skip();
                (
                    T::default(),
                    Some(type_err_msg(
                        t,
                        go_type,
                        "cannot decode CBOR array to struct without toarray option".to_owned(),
                    )),
                )
            }
            CborType::PositiveInt => {
                self.get_head();
                (T::default(), Some(type_err(t, go_type)))
            }
            CborType::NegativeInt => {
                let (_, _, val) = self.get_head();
                if val > i64::MAX as u64 {
                    let dec = bignum_dec(&val.to_be_bytes(), true);
                    (
                        T::default(),
                        Some(type_err_msg(
                            t,
                            go_type,
                            format!("{dec} overflows Go's int64"),
                        )),
                    )
                } else {
                    (T::default(), Some(type_err(t, go_type)))
                }
            }
            CborType::ByteString => {
                self.parse_byte_string();
                (T::default(), Some(type_err(t, go_type)))
            }
            CborType::TextString => match self.parse_text_string() {
                Err(e) => (T::default(), Some(e)),
                Ok(_) => (T::default(), Some(type_err(t, go_type))),
            },
            CborType::Primitives => {
                let (_, ai, _) = self.get_head();
                match ai {
                    22 | 23 => (T::default(), None), // zero value
                    _ => (T::default(), Some(type_err(t, go_type))),
                }
            }
            CborType::Tag => {
                let (_, _, num) = self.get_head();
                match num {
                    2 | 3 => {
                        let b = self.parse_byte_string();
                        let fits = match bignum_u64(&b) {
                            Some(x) => num == 2 || x <= i64::MAX as u64,
                            None => false,
                        };
                        if fits {
                            (T::default(), Some(type_err(CborType::Tag, go_type)))
                        } else {
                            let dec = bignum_dec(&b, num == 3);
                            (
                                T::default(),
                                Some(type_err_msg(
                                    CborType::Tag,
                                    go_type,
                                    format!("{dec} overflows {go_type}"),
                                )),
                            )
                        }
                    }
                    _ => self.parse_to_struct(go_type, map),
                }
            }
        }
    }

    /// The `parseMapToStruct` mirror for a `keyasint` struct with integer
    /// keys `0..nfields` (at most 64): the first occurrence of a key goes to
    /// `field`, which decodes the value and returns its error already wrapped
    /// with the field's name; duplicates and unknown keys are skipped; text
    /// keys never match; other key types are diagnosed but tolerated. The
    /// first error wins and decoding continues, as in Go. The same loop as
    /// [`Dec::parse_map_to_entry`], which predates this helper.
    fn parse_map_to_struct(
        &mut self,
        nfields: u64,
        mut field: impl FnMut(&mut Self, u64) -> Option<CborError>,
    ) -> Option<CborError> {
        debug_assert!(nfields <= 64, "`found` is a 64-bit mask");
        let (_, ai, val) = self.get_head();
        let indef = ai == 31;
        let mut found = 0u64;
        let mut first: Option<CborError> = None;
        let record = |first: &mut Option<CborError>, err: Option<CborError>| {
            if first.is_none() {
                *first = err;
            }
        };
        let mut i = 0u64;
        loop {
            if indef {
                if self.found_break() {
                    break;
                }
            } else if i >= val {
                break;
            }
            let kt = self.next_type();
            match kt {
                CborType::PositiveInt => {
                    let (_, _, k) = self.get_head();
                    if k > i64::MAX as u64 {
                        record(
                            &mut first,
                            Some(type_err_msg(
                                kt,
                                "int64",
                                format!("{k} overflows Go's int64"),
                            )),
                        );
                        self.skip();
                    } else if k < nfields && found & (1 << k) == 0 {
                        found |= 1 << k;
                        let err = field(self, k);
                        record(&mut first, err);
                    } else {
                        // Unknown positive key, or a duplicate: skip value.
                        self.skip();
                    }
                }
                CborType::NegativeInt => {
                    let (_, _, k) = self.get_head();
                    if k > i64::MAX as u64 {
                        record(
                            &mut first,
                            Some(type_err_msg(
                                kt,
                                "int64",
                                format!("-1-{k} overflows Go's int64"),
                            )),
                        );
                    }
                    // Negative keys never match a field: skip value.
                    self.skip();
                }
                CborType::TextString => {
                    if let Err(err) = self.parse_text_string() {
                        record(&mut first, Some(err));
                    }
                    // keyasint fields are not matchable by name.
                    self.skip(); // value
                }
                _ => {
                    record(
                        &mut first,
                        Some(type_err_msg(
                            kt,
                            "string",
                            format!(
                                "map key is of type {kt} and cannot be used to match struct field name"
                            ),
                        )),
                    );
                    self.skip(); // key
                    self.skip(); // value
                }
            }
            i += 1;
        }
        first
    }

    /// Decodes into Go `commit.wireIdentity`.
    fn parse_to_wire_identity(&mut self) -> (WireIdentity, Option<CborError>) {
        self.parse_to_struct("commit.wireIdentity", |d| {
            let mut w = WireIdentity::default();
            let err = d.parse_map_to_struct(4, |d, k| {
                let err = match k {
                    0 => {
                        let (v, err) = d.parse_to_string();
                        w.name = v;
                        err
                    }
                    1 => {
                        let (v, err) = d.parse_to_string();
                        w.email = v;
                        err
                    }
                    2 => {
                        let (v, err) = d.parse_to_i64();
                        w.when = v;
                        err
                    }
                    _ => {
                        let (v, err) = d.parse_to_i64();
                        w.tz_offset = v;
                        err
                    }
                };
                err.map(|e| wrap_field(e, WIRE_IDENTITY_FIELDS[k as usize]))
            });
            (w, err)
        })
    }

    /// Decodes into Go `commit.wireCommit`. A type error inside a nested
    /// identity leaves here carrying the **outer** field name
    /// (`commit.wireCommit.2`/`.3`) with the inner Go type: fxamacker's two
    /// wrap sites both overwrite `StructFieldName` on the way out.
    fn parse_to_wire_commit(&mut self) -> (WireCommit, Option<CborError>) {
        self.parse_to_struct("commit.wireCommit", |d| {
            let mut w = WireCommit::default();
            let err = d.parse_map_to_struct(7, |d, k| {
                let err = match k {
                    0 => {
                        let (v, err) = d.parse_to_bytes("[]uint8");
                        w.tree = v;
                        err
                    }
                    1 => {
                        let (v, err) = d.parse_to_byte_slices();
                        w.parents = v;
                        err
                    }
                    2 => {
                        let (v, err) = d.parse_to_wire_identity();
                        w.author = v;
                        err
                    }
                    3 => {
                        let (v, err) = d.parse_to_wire_identity();
                        w.committer = v;
                        err
                    }
                    4 => {
                        let (v, err) = d.parse_to_string();
                        w.message = v;
                        err
                    }
                    5 => {
                        let (v, err) = d.parse_to_bytes("[]uint8");
                        w.signature = v;
                        err
                    }
                    _ => {
                        let (v, err) = d.parse_to_bytes("[]uint8");
                        w.public_key = v;
                        err
                    }
                };
                err.map(|e| wrap_field(e, WIRE_COMMIT_FIELDS[k as usize]))
            });
            (w, err)
        })
    }

    fn parse_to_byte_slices(&mut self) -> (Vec<Vec<u8>>, Option<CborError>) {
        self.parse_to_slice_of("[][]uint8", |d| d.parse_to_bytes("[]uint8"))
    }

    fn parse_to_entries(&mut self) -> (Vec<Entry>, Option<CborError>) {
        self.parse_to_slice_of("[]fstree.Entry", |d| d.parse_to_entry())
    }

    fn parse_to_pairs(&mut self) -> (Vec<DirPair>, Option<CborError>) {
        self.parse_to_slice_of("[]fstree.DirPair", |d| d.parse_to_pair())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bignum_decimal_rendering() {
        assert_eq!(bignum_dec(&[], false), "0");
        assert_eq!(bignum_dec(&[0, 0], false), "0");
        assert_eq!(bignum_dec(&[1, 0], false), "256");
        assert_eq!(bignum_dec(&[0xff; 8], false), "18446744073709551615");
        // -(x+1): tag-3 semantics.
        assert_eq!(bignum_dec(&[], true), "-1");
        assert_eq!(bignum_dec(&[0xff; 8], true), "-18446744073709551616");
        assert_eq!(
            bignum_dec(&[1, 0, 0, 0, 0, 0, 0, 0, 0], false),
            "18446744073709551616"
        );
    }

    #[test]
    fn bignum_u64_fit() {
        assert_eq!(bignum_u64(&[]), Some(0));
        assert_eq!(bignum_u64(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 5]), Some(5));
        assert_eq!(bignum_u64(&[0xff; 8]), Some(u64::MAX));
        assert_eq!(bignum_u64(&[1, 0, 0, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn wellformed_limits_and_syntax() {
        // Depth 33 nested arrays exceeds the default 32.
        let mut deep = vec![0x81u8; 33];
        deep.push(0x40);
        assert_eq!(
            wellformed(&deep, &DECODE_WF),
            Err(CborError::MaxNestedLevel(32))
        );
        // Depth 32 passes well-formedness.
        let mut ok = vec![0x81u8; 32];
        ok.push(0x40);
        assert_eq!(wellformed(&ok, &DECODE_WF), Ok(()));
        // The marshaler mode allows far deeper nesting.
        assert_eq!(wellformed(&deep, &MARSHALER_WF), Ok(()));
        // Array count 131073 exceeds the element cap without reading elements.
        assert_eq!(
            wellformed(&[0x9a, 0x00, 0x02, 0x00, 0x01], &DECODE_WF),
            Err(CborError::MaxArrayElements(131072))
        );
        // Indefinite lengths are forbidden in the marshaler mode only.
        assert_eq!(wellformed(&[0x9f, 0xff], &DECODE_WF), Ok(()));
        assert_eq!(
            wellformed(&[0x9f, 0xff], &MARSHALER_WF),
            Err(CborError::IndefiniteLengthNotAllowed(CborType::Array))
        );
        // Builtin tag content is checked in the marshaler mode only.
        let tagged = [0xc0, 0x00]; // tag 0 + uint
        assert_eq!(wellformed(&tagged, &DECODE_WF), Ok(()));
        assert_eq!(
            wellformed(&tagged, &MARSHALER_WF),
            Err(CborError::InadmissibleTagContent {
                tag: "0",
                expected: "text string",
                got: CborType::PositiveInt,
            })
        );
        // Chained tags count toward nesting after the first.
        let mut chain = vec![0xd9, 0xd9, 0xf7]; // 55799 then 33 more tags
        chain.extend_from_slice(&[0xc1; 33]);
        chain.push(0x00);
        assert_eq!(
            wellformed(&chain, &DECODE_WF),
            Err(CborError::MaxNestedLevel(32))
        );
    }
}
