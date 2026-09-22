//! The named-pointer record: a global name pointing at a store key, with
//! creator, creation time, and an optional opaque signature (Go package
//! `reference`).
//!
//! Encoding is RFC 8949 §4.2 core-deterministic CBOR, matching the fstree
//! object convention (canonical map, integer keys). See
//! `architecture/references.md`.

use crate::cbor::{self, MAJOR_ARRAY, MAJOR_BSTR, MAJOR_MAP, MAJOR_NEGINT, MAJOR_TSTR, MAJOR_UINT};
use crate::key;

/// The maximum reference name length in bytes (Go: `MaxNameLen`).
pub const MAX_NAME_LEN: usize = 1024;

/// The maximum `user` field length in bytes (Go: `MaxUserLen`).
pub const MAX_USER_LEN: usize = 1024;

/// The maximum `signature` field length in bytes, 64 KiB (Go:
/// `MaxSignatureLen`).
pub const MAX_SIGNATURE_LEN: usize = 64 << 10;

/// The maximum `public_key` field length in bytes, 16 KiB (Go:
/// `MaxPublicKeyLen`).
pub const MAX_PUBLIC_KEY_LEN: usize = 16 << 10;

/// A name-rule violation from [`validate_name`]. `Display` messages reproduce
/// the Go package's diagnostics verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// The name is empty.
    #[error("reference name must not be empty")]
    Empty,
    /// The name exceeds [`MAX_NAME_LEN`] bytes.
    #[error("reference name exceeds {} bytes", MAX_NAME_LEN)]
    TooLong,
    /// The name is not valid UTF-8. Unreachable through [`validate_name`]
    /// (`&str` is UTF-8 by construction — Go strings are not); the variant is
    /// kept so the Go rule and its diagnostic stay documented, and because the
    /// decode path enforces the same rule (as a [`DecodeError::InvalidUtf8`],
    /// where Go also rejects it during unmarshalling).
    #[error("reference name must be valid UTF-8")]
    NotUtf8,
    /// The name contains `'@'`, the ref/path separator.
    #[error("reference name must not contain '@'")]
    AtSign,
    /// The name contains a control character (< 0x20 or 0x7F).
    #[error("reference name must not contain control characters")]
    ControlChar,
}

/// A user-rule violation from [`validate_user`]. `Display` messages reproduce
/// the Go package's diagnostics verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UserError {
    /// The user string is empty.
    #[error("user must not be empty")]
    Empty,
    /// The user string exceeds [`MAX_USER_LEN`] bytes.
    #[error("user exceeds {} bytes", MAX_USER_LEN)]
    TooLong,
    /// The user string is not valid UTF-8. Unreachable through
    /// [`validate_user`]; see [`NameError::NotUtf8`].
    #[error("user must be valid UTF-8")]
    NotUtf8,
    /// The user string contains a control character (< 0x20 or 0x7F).
    #[error("user must not contain control characters")]
    ControlChar,
}

fn cbor_type_name(major: u8) -> &'static str {
    // fxamacker/cbor's cborType.String() names, so decode diagnostics read
    // like the Go ones.
    match major {
        0 => "positive integer",
        1 => "negative integer",
        2 => "byte string",
        3 => "UTF-8 text string",
        4 => "array",
        5 => "map",
        6 => "tag",
        _ => "primitives",
    }
}

fn sign_word(negative: bool) -> &'static str {
    if negative { "negative" } else { "positive" }
}

fn bad_tag_content_msg(tag: u64, got: u8) -> String {
    // fxamacker's InadmissibleTagContentTypeError messages, verbatim.
    let got = cbor_type_name(got);
    match tag {
        0 => format!("cbor: tag number 0 must be followed by text string, got {got}"),
        1 => format!(
            "cbor: tag number 1 must be followed by integer or floating-point number, got {got}"
        ),
        _ => format!("cbor: tag number 2 or 3 must be followed by byte string, got {got}"),
    }
}

/// Decode-stage errors: the input could not be parsed as a CBOR map of the
/// record's shape. In Go these come from `fxamacker/cbor`'s `Unmarshal`; the
/// three-way classification (decode error vs. validation error vs.
/// non-canonical) is ported exactly, while the message text approximates
/// fxamacker's diagnostics. See `port-notes/reference.md` for the known
/// classification edge cases (all rejections either way).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// A CBOR head could not be read (truncated input, or a reserved /
    /// indefinite-length head, which this codec never emits).
    #[error(transparent)]
    Cbor(#[from] cbor::Error),
    /// The top-level item is not a map.
    #[error("cbor: cannot unmarshal {} into reference record", cbor_type_name(*got))]
    NotAMap {
        /// The major type actually found.
        got: u8,
    },
    /// A known field's value has the wrong CBOR type.
    #[error("cbor: cannot unmarshal {} into reference field {field}", cbor_type_name(*got))]
    WrongFieldType {
        /// The integer map key (0..=5).
        field: u8,
        /// The major type actually found.
        got: u8,
    },
    /// A text-string field is not valid UTF-8.
    #[error("cbor: invalid UTF-8 string")]
    InvalidUtf8,
    /// `created_at` (key 3) does not fit an `i64`.
    #[error("cbor: {} integer overflows int64 in reference field 3", sign_word(*negative))]
    IntOverflow {
        /// Whether the offending integer was a CBOR negative integer.
        negative: bool,
    },
    /// A map key of a type that can never match a struct field. Verified
    /// against fxamacker: unsigned/negative-integer and text-string keys that
    /// match no field are skipped quietly, while byte-string, array, map,
    /// tag, and primitive (bool/null/float) keys are an unmarshal error.
    #[error(
        "cbor: map key is of type {} and cannot be used to match struct field name",
        cbor_type_name(*got)
    )]
    BadMapKeyType {
        /// The major type of the offending key.
        got: u8,
    },
    /// Skipping an unknown key's value exceeded the nesting cap
    /// (fxamacker's default `MaxNestedLevels`, 32).
    #[error("cbor: exceeded max nested level {}", MAX_NESTED_LEVELS)]
    MaxNestedLevels,
    /// A two-byte simple-value head (`0xF8` with argument < 32), which
    /// RFC 8949 §3.3 declares not well-formed. fxamacker rejects it in the
    /// well-formedness pass, anywhere in the document; the message is
    /// reproduced verbatim.
    #[error("cbor: invalid simple value {0} for type primitives")]
    InvalidSimpleValue(u8),
    /// An integer map key whose value does not fit an `i64`: fxamacker parses
    /// integer keys as `int64` before field matching and errors on overflow.
    /// The message reproduces fxamacker's `UnmarshalTypeError` verbatim.
    #[error(
        "cbor: cannot unmarshal {} into Go value of type int64 ({}{} overflows Go's int64)",
        cbor_type_name(u8::from(*negative)),
        if *negative { "-1-" } else { "" },
        arg
    )]
    IntKeyOverflow {
        /// Whether the key was a CBOR negative integer.
        negative: bool,
        /// The head argument (the value is `arg` or `-1 - arg`).
        arg: u64,
    },
    /// A built-in tag (0–3) whose content has the wrong CBOR type
    /// (fxamacker's `InadmissibleTagContentTypeError`, messages verbatim).
    /// Checked only where fxamacker decodes a value (matched fields and the
    /// top level), not on skipped items.
    #[error("{}", bad_tag_content_msg(*tag, *got))]
    BadTagContent {
        /// The offending built-in tag number (0–3).
        tag: u64,
        /// The major type of the tag's content.
        got: u8,
    },
    /// A bignum tag (2/3) on `created_at` whose value does not fit an `i64`.
    #[error("cbor: {} bignum overflows int64 in reference field 3", sign_word(*negative))]
    BignumOverflow {
        /// Whether the bignum was tag 3 (negative).
        negative: bool,
    },
    /// An element of a CBOR array decoded into a byte-vector field has a type
    /// that cannot become a byte (fxamacker decodes arrays into `[]byte`
    /// element-wise, like `encoding/json`).
    #[error(
        "cbor: cannot unmarshal {} into byte element of reference field {field}",
        cbor_type_name(*got)
    )]
    ByteElemType {
        /// The byte-vector field's integer key (1, 4, or 5).
        field: u8,
        /// The major type of the offending element.
        got: u8,
    },
    /// An element of a CBOR array decoded into a byte-vector field is an
    /// integer (or bignum) that does not fit a byte.
    #[error("cbor: integer overflows uint8 in byte element of reference field {field}")]
    ByteElemOverflow {
        /// The byte-vector field's integer key (1, 4, or 5).
        field: u8,
    },
    /// Input remained after the complete map.
    #[error("cbor: {n} bytes of extraneous data starting at index {index}")]
    Extraneous {
        /// Number of unread bytes.
        n: usize,
        /// Byte offset where the extraneous data starts.
        index: usize,
    },
}

/// Errors from encoding, decoding, and validating reference records.
///
/// The validation variants (`Name`, `Key`, `User`, `SignatureTooLong`,
/// `PublicKeyTooLong`) are returned bare from [`Reference::encode`], exactly
/// as Go's `Encode` returns `validate()`'s error; [`Reference::decode`] wraps
/// the same failures in [`Error::Invalid`], mirroring Go's
/// `"invalid reference: %w"`. `Display` messages reproduce the Go
/// diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The name failed [`validate_name`] (returned unwrapped, as in Go).
    #[error(transparent)]
    Name(#[from] NameError),
    /// The key is not a canonical 32-byte store key.
    #[error("reference key: {0}")]
    Key(#[source] key::Error),
    /// A non-empty user failed [`validate_user`].
    #[error("reference user: {0}")]
    User(#[source] UserError),
    /// The signature exceeds [`MAX_SIGNATURE_LEN`] bytes.
    #[error("reference signature exceeds {} bytes", MAX_SIGNATURE_LEN)]
    SignatureTooLong,
    /// The public key exceeds [`MAX_PUBLIC_KEY_LEN`] bytes.
    #[error("reference public key exceeds {} bytes", MAX_PUBLIC_KEY_LEN)]
    PublicKeyTooLong,
    /// The input could not be unmarshalled (Go: `"decoding reference: %w"`).
    #[error("decoding reference: {0}")]
    Decode(#[source] DecodeError),
    /// The record decoded but failed validation (Go:
    /// `"invalid reference: %w"`).
    #[error("invalid reference: {0}")]
    Invalid(#[source] Box<Error>),
    /// The record decoded and validated, but its bytes are not the canonical
    /// deterministic encoding (extra map keys, non-minimal heads, wrong key
    /// order, ...).
    #[error("reference encoding is not canonical")]
    NotCanonical,
}

/// Checks the reference-name rules: 1..=[`MAX_NAME_LEN`] bytes of valid
/// UTF-8, no `'@'` (the ref/path separator) and no control characters. `'/'`
/// is allowed; names are opaque strings with no structural meaning.
///
/// Go's "must be valid UTF-8" rule is enforced by `&str` itself here; see
/// [`NameError::NotUtf8`].
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(NameError::TooLong);
    }
    for r in name.chars() {
        if r == '@' {
            return Err(NameError::AtSign);
        }
        if r < '\u{20}' || r == '\u{7f}' {
            return Err(NameError::ControlChar);
        }
    }
    Ok(())
}

/// Checks the user-identity rules used both by config-user and by
/// [`Reference`] records: 1..=[`MAX_USER_LEN`] bytes of valid UTF-8 with no
/// control characters. `'@'` is explicitly allowed so that e-mail addresses
/// are valid.
///
/// Note: an empty `user` in a [`Reference`] record remains valid at the
/// record level (`validate_user` is only called from validation when
/// `user` is non-empty). `validate_user` itself rejects empty so that
/// config-user always stores a usable identity.
pub fn validate_user(user: &str) -> Result<(), UserError> {
    if user.is_empty() {
        return Err(UserError::Empty);
    }
    if user.len() > MAX_USER_LEN {
        return Err(UserError::TooLong);
    }
    for r in user.chars() {
        if r < '\u{20}' || r == '\u{7f}' {
            return Err(UserError::ControlChar);
        }
    }
    Ok(())
}

/// The record stored under a name. Fields are encoded as a canonical CBOR map
/// with integer keys 0–5; `signature` (key 4) and `public_key` (key 5) are
/// omitted when absent. The signature payload is the encoding without key 4
/// only, so a signature covers the public key it was made with.
///
/// The byte-vector fields carry Go's `omitempty` semantics: an **empty**
/// `signature`/`public_key` means absent (the key is omitted from the
/// encoding). A present-but-empty field is not representable — exactly as in
/// Go, where nil and zero-length slices are both omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reference {
    /// Global reference name (CBOR key 0).
    pub name: String,
    /// 32-byte canonical store key (CBOR key 1).
    pub key: Vec<u8>,
    /// Creator identity (CBOR key 2); may be empty.
    pub user: String,
    /// Creation time, ns since the Unix epoch (CBOR key 3).
    pub created_at: i64,
    /// Opaque signature, raw SSHSIG blob (CBOR key 4); empty = absent.
    pub signature: Vec<u8>,
    /// Signer's public key, SSH wire format (CBOR key 5); empty = absent.
    pub public_key: Vec<u8>,
}

impl Reference {
    /// Checks the whole record: name rules plus a canonical key, and bounds
    /// on the user, signature, and public-key fields (Go: `validate`).
    fn validate(&self) -> Result<(), Error> {
        validate_name(&self.name).map_err(Error::Name)?;
        key::Key::parse(&self.key).map_err(Error::Key)?;
        if !self.user.is_empty() {
            validate_user(&self.user).map_err(Error::User)?;
        }
        if self.signature.len() > MAX_SIGNATURE_LEN {
            return Err(Error::SignatureTooLong);
        }
        if self.public_key.len() > MAX_PUBLIC_KEY_LEN {
            return Err(Error::PublicKeyTooLong);
        }
        Ok(())
    }

    /// The canonical encoding, without validating first. Infallible — unlike
    /// Go's `encMode.Marshal` there is no error path, so Go's unreachable
    /// `"re-encoding reference: %w"` wrap has no counterpart here.
    fn encode_unchecked(&self) -> Vec<u8> {
        let mut pairs = 4u64;
        if !self.signature.is_empty() {
            pairs += 1;
        }
        if !self.public_key.is_empty() {
            pairs += 1;
        }
        let mut b = Vec::new();
        cbor::append_head(&mut b, MAJOR_MAP, pairs);
        cbor::append_head(&mut b, MAJOR_UINT, 0);
        cbor::append_tstr(&mut b, &self.name);
        cbor::append_head(&mut b, MAJOR_UINT, 1);
        cbor::append_bstr(&mut b, &self.key);
        cbor::append_head(&mut b, MAJOR_UINT, 2);
        cbor::append_tstr(&mut b, &self.user);
        cbor::append_head(&mut b, MAJOR_UINT, 3);
        cbor::append_int(&mut b, self.created_at);
        if !self.signature.is_empty() {
            cbor::append_head(&mut b, MAJOR_UINT, 4);
            cbor::append_bstr(&mut b, &self.signature);
        }
        if !self.public_key.is_empty() {
            cbor::append_head(&mut b, MAJOR_UINT, 5);
            cbor::append_bstr(&mut b, &self.public_key);
        }
        b
    }

    /// Returns the deterministic CBOR encoding of a validated record (Go:
    /// `Encode`).
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        Ok(self.encode_unchecked())
    }

    /// Parses and validates a record (Go: `Decode`). It rejects non-canonical
    /// encodings: the input must be byte-for-byte identical to what
    /// [`Reference::encode`] would produce for the same record (extra map
    /// keys, indefinite-length items, and non-minimal integer/length
    /// encodings are all rejected).
    pub fn decode(b: &[u8]) -> Result<Reference, Error> {
        let r = unmarshal(b).map_err(Error::Decode)?;
        if let Err(e) = r.validate() {
            return Err(Error::Invalid(Box::new(e)));
        }
        if r.encode_unchecked() != b {
            return Err(Error::NotCanonical);
        }
        Ok(r)
    }

    /// Returns the bytes a signature runs over: the deterministic encoding of
    /// the record without its `signature` field (Go: `SignaturePayload`).
    /// `public_key` stays in, so the payload binds the signer's key; set it
    /// before computing the payload.
    pub fn signature_payload(&self) -> Result<Vec<u8>, Error> {
        let mut r = self.clone();
        r.signature = Vec::new();
        r.encode()
    }
}

/// Nesting cap when skipping unknown-key values, mirroring fxamacker/cbor's
/// default `MaxNestedLevels`.
const MAX_NESTED_LEVELS: usize = 32;

/// CBOR simple-value arguments for null (0xF6) and undefined (0xF7), which
/// fxamacker decodes to the field's zero value.
const SIMPLE_NULL: u64 = 22;
const SIMPLE_UNDEFINED: u64 = 23;

fn is_null_head(major: u8, arg: u64, head_len: usize) -> bool {
    // Only the one-byte heads 0xF6/0xF7 are null/undefined; longer major-7
    // heads with the same argument are float16 payloads.
    major == 7 && head_len == 1 && (arg == SIMPLE_NULL || arg == SIMPLE_UNDEFINED)
}

/// Skips one complete CBOR item, checking well-formedness exactly the way
/// fxamacker's `wellformedInternal` does (depth starts at 0; entering an
/// array or map increments-then-checks against [`MAX_NESTED_LEVELS`]; in a
/// chain of nested tags the first tag adds no level and each additional tag
/// adds one, scanned iteratively). The depth cap keeps hostile nesting from
/// overflowing the stack. The tag-number boundaries were verified
/// differentially: 32 nested arrays under the top-level map are rejected
/// while 31 pass, and 33 nested tags are rejected while 32 pass.
fn skip_item(b: &[u8], depth: usize) -> Result<&[u8], DecodeError> {
    let (major, n, rest) = cbor::read_head(b)?;
    match major {
        MAJOR_BSTR | MAJOR_TSTR => {
            if (rest.len() as u64) < n {
                return Err(cbor::Error::UnexpectedEof.into());
            }
            Ok(&rest[n as usize..])
        }
        MAJOR_ARRAY | MAJOR_MAP => {
            let depth = depth + 1;
            if depth > MAX_NESTED_LEVELS {
                return Err(DecodeError::MaxNestedLevels);
            }
            let items = if major == MAJOR_MAP { 2 } else { 1 };
            let mut rest = rest;
            for _ in 0..n {
                for _ in 0..items {
                    rest = skip_item(rest, depth)?;
                }
            }
            Ok(rest)
        }
        MAJOR_TAG => {
            // Scan the nested tag numbers iteratively, as fxamacker does: a
            // tag number must be followed by tag content, and every tag after
            // the first adds a nesting level.
            let mut depth = depth;
            let mut rest = rest;
            loop {
                let Some(&next) = rest.first() else {
                    return Err(cbor::Error::UnexpectedEof.into());
                };
                if next >> 5 != MAJOR_TAG {
                    break;
                }
                let (_, _, r) = cbor::read_head(rest)?;
                rest = r;
                depth += 1;
                if depth > MAX_NESTED_LEVELS {
                    return Err(DecodeError::MaxNestedLevels);
                }
            }
            // The tag content.
            skip_item(rest, depth)
        }
        // Integers and simple values / floats: the head carries everything.
        _ => {
            // fxamacker's well-formedness pass rejects two-byte simple values
            // < 32 (0xF8 0x00..=0x1F), per RFC 8949 §3.3.
            if major == 7 && b.len() - rest.len() == 2 && n < 32 {
                return Err(DecodeError::InvalidSimpleValue(n as u8));
            }
            Ok(rest)
        }
    }
}

/// The self-described-CBOR tag number (RFC 8949 §3.4.6), which fxamacker
/// strips transparently wherever it decodes a value.
const TAG_SELF_DESCRIBED: u64 = 55799;

/// Peeks a tag head: the tag number and the input after the head, if `b`
/// starts with a readable major-6 head.
fn peek_tag(b: &[u8]) -> Option<(u64, &[u8])> {
    if b.first()? >> 5 != MAJOR_TAG {
        return None;
    }
    let (_, n, rest) = cbor::read_head(b).ok()?;
    Some((n, rest))
}

/// Mirrors fxamacker's `parseToValue` tag preamble, run wherever a value is
/// decoded (matched struct fields and the top level, not skipped items):
/// strips any leading self-described-CBOR tags (55799), then validates every
/// remaining tag number in the chain against the CBOR type of its immediate
/// content (built-in tags 0–3), without consuming the chain.
fn strip_and_check_tags(b: &[u8]) -> Result<&[u8], DecodeError> {
    let mut b = b;
    while let Some((TAG_SELF_DESCRIBED, rest)) = peek_tag(b) {
        b = rest;
    }
    let mut cur = b;
    while let Some((tag, rest)) = peek_tag(cur) {
        let Some(&content) = rest.first() else {
            return Err(cbor::Error::UnexpectedEof.into());
        };
        let got = content >> 5;
        let ok = match tag {
            0 => got == MAJOR_TSTR,
            1 => got == MAJOR_UINT || got == MAJOR_NEGINT || (0xf9..=0xfb).contains(&content),
            2 | 3 => got == MAJOR_BSTR,
            _ => true,
        };
        if !ok {
            return Err(DecodeError::BadTagContent { tag, got });
        }
        cur = rest;
    }
    Ok(b)
}

/// Reads a bignum tag's byte-string content (the head has already been
/// checked to be a byte string by [`strip_and_check_tags`]).
fn read_bignum_content(b: &[u8], tag: u64) -> Result<(&[u8], &[u8]), DecodeError> {
    let (major, n, rest) = cbor::read_head(b)?;
    if major != MAJOR_BSTR {
        // Unreachable: the chain validation already required a byte string.
        return Err(DecodeError::BadTagContent { tag, got: major });
    }
    if (rest.len() as u64) < n {
        return Err(cbor::Error::UnexpectedEof.into());
    }
    let n = n as usize;
    Ok((&rest[..n], &rest[n..]))
}

/// The magnitude of a bignum's content bytes as a `u64`, or `None` if it
/// exceeds 64 bits (leading zero bytes are insignificant, as in `big.Int`).
fn bignum_magnitude(content: &[u8]) -> Option<u64> {
    let Some(first) = content.iter().position(|&x| x != 0) else {
        return Some(0); // empty or all-zero content is the bignum 0
    };
    let digits = &content[first..];
    if digits.len() > 8 {
        return None;
    }
    let mut v = 0u64;
    for &byte in digits {
        v = v << 8 | u64::from(byte);
    }
    Some(v)
}

/// CBOR major type 6: tag (not part of the shared `cbor` module's constants
/// because the deterministic codecs never emit tags).
const MAJOR_TAG: u8 = 6;

fn read_tstr_field(b: &[u8], field: u8) -> Result<(String, &[u8]), DecodeError> {
    let b = strip_and_check_tags(b)?;
    let (major, n, rest) = cbor::read_head(b)?;
    if major == MAJOR_TAG {
        // fxamacker: a bignum can never fill a string; any other tag is
        // unwrapped transparently and its content decoded.
        if n == 2 || n == 3 {
            return Err(DecodeError::WrongFieldType {
                field,
                got: MAJOR_TAG,
            });
        }
        return read_tstr_field(rest, field);
    }
    if is_null_head(major, n, b.len() - rest.len()) {
        return Ok((String::new(), rest));
    }
    if major != MAJOR_TSTR {
        return Err(DecodeError::WrongFieldType { field, got: major });
    }
    if (rest.len() as u64) < n {
        return Err(cbor::Error::UnexpectedEof.into());
    }
    let n = n as usize;
    let s = std::str::from_utf8(&rest[..n]).map_err(|_| DecodeError::InvalidUtf8)?;
    Ok((s.to_owned(), &rest[n..]))
}

fn read_bstr_field(b: &[u8], field: u8) -> Result<(Vec<u8>, &[u8]), DecodeError> {
    let b = strip_and_check_tags(b)?;
    let (major, n, rest) = cbor::read_head(b)?;
    if major == MAJOR_TAG {
        return match n {
            // fxamacker fills a []byte field with a bignum tag's raw content
            // bytes.
            2 | 3 => {
                let (content, rest) = read_bignum_content(rest, n)?;
                Ok((content.to_vec(), rest))
            }
            _ => read_bstr_field(rest, field),
        };
    }
    if is_null_head(major, n, b.len() - rest.len()) {
        return Ok((Vec::new(), rest));
    }
    match major {
        MAJOR_BSTR => {
            if (rest.len() as u64) < n {
                return Err(cbor::Error::UnexpectedEof.into());
            }
            let n = n as usize;
            Ok((rest[..n].to_vec(), &rest[n..]))
        }
        // fxamacker decodes a CBOR array into []byte element-wise, like
        // encoding/json.
        MAJOR_ARRAY => {
            let mut out = Vec::with_capacity(n.min(rest.len() as u64) as usize);
            let mut rest = rest;
            for _ in 0..n {
                let (byte, r) = read_byte_elem(rest, field)?;
                out.push(byte);
                rest = r;
            }
            Ok((out, rest))
        }
        _ => Err(DecodeError::WrongFieldType { field, got: major }),
    }
}

/// Decodes one CBOR array element into a byte, mirroring fxamacker's
/// `parseToValue` into a `uint8`: unsigned integers and unassigned simple
/// values in range fill directly, null/undefined become 0, bignum tags follow
/// the big.Int paths, other tags unwrap, and everything else is a type error.
fn read_byte_elem(b: &[u8], field: u8) -> Result<(u8, &[u8]), DecodeError> {
    let b = strip_and_check_tags(b)?;
    let (major, n, rest) = cbor::read_head(b)?;
    match major {
        MAJOR_UINT => {
            if n > u64::from(u8::MAX) {
                return Err(DecodeError::ByteElemOverflow { field });
            }
            Ok((n as u8, rest))
        }
        MAJOR_TAG => match n {
            2 => {
                let (content, rest) = read_bignum_content(rest, n)?;
                match bignum_magnitude(content) {
                    Some(v) if v <= u64::from(u8::MAX) => Ok((v as u8, rest)),
                    _ => Err(DecodeError::ByteElemOverflow { field }),
                }
            }
            3 => {
                // A negative bignum can never fill a uint8: a type error when
                // it fits int64, an overflow otherwise (fxamacker).
                let (content, _) = read_bignum_content(rest, n)?;
                match bignum_magnitude(content) {
                    Some(m) if m <= i64::MAX as u64 => Err(DecodeError::ByteElemType {
                        field,
                        got: MAJOR_TAG,
                    }),
                    _ => Err(DecodeError::ByteElemOverflow { field }),
                }
            }
            _ => read_byte_elem(rest, field),
        },
        7 => match b.len() - rest.len() {
            1 => match n {
                20 | 21 => Err(DecodeError::ByteElemType { field, got: 7 }), // bool
                22 | 23 => Ok((0, rest)),                                    // null/undefined
                _ => Ok((n as u8, rest)), // unassigned simple value 0..=19
            },
            // Simple value 32..=255 (0xF8 0x00..=0x1F was rejected in pass 1).
            2 => Ok((n as u8, rest)),
            // Floats never fill an integer type.
            _ => Err(DecodeError::ByteElemType { field, got: 7 }),
        },
        _ => Err(DecodeError::ByteElemType { field, got: major }),
    }
}

fn read_int64_field(b: &[u8]) -> Result<(i64, &[u8]), DecodeError> {
    let b = strip_and_check_tags(b)?;
    let (major, n, rest) = cbor::read_head(b)?;
    match major {
        MAJOR_UINT => {
            if n > i64::MAX as u64 {
                return Err(DecodeError::IntOverflow { negative: false });
            }
            Ok((n as i64, rest))
        }
        MAJOR_NEGINT => {
            // Value is -1 - n; representable iff n <= i64::MAX.
            if n > i64::MAX as u64 {
                return Err(DecodeError::IntOverflow { negative: true });
            }
            Ok((-1 - (n as i64), rest))
        }
        MAJOR_TAG => match n {
            2 => {
                let (content, rest) = read_bignum_content(rest, n)?;
                match bignum_magnitude(content) {
                    Some(v) if v <= i64::MAX as u64 => Ok((v as i64, rest)),
                    _ => Err(DecodeError::BignumOverflow { negative: false }),
                }
            }
            3 => {
                let (content, rest) = read_bignum_content(rest, n)?;
                match bignum_magnitude(content) {
                    Some(m) if m <= i64::MAX as u64 => Ok((-1 - (m as i64), rest)),
                    _ => Err(DecodeError::BignumOverflow { negative: true }),
                }
            }
            _ => read_int64_field(rest),
        },
        7 => match b.len() - rest.len() {
            1 => match n {
                20 | 21 => Err(DecodeError::WrongFieldType { field: 3, got: 7 }), // bool
                22 | 23 => Ok((0, rest)),                                         // null/undefined
                // fxamacker fills unassigned simple values into integer
                // fields as their numeric value.
                _ => Ok((n as i64, rest)),
            },
            2 => Ok((n as i64, rest)), // simple value 32..=255
            _ => Err(DecodeError::WrongFieldType { field: 3, got: 7 }), // floats
        },
        _ => Err(DecodeError::WrongFieldType {
            field: 3,
            got: major,
        }),
    }
}

/// The lax unmarshal step of [`Reference::decode`], mirroring Go's
/// `cbor.Unmarshal` into the keyasint struct. Like fxamacker, it first checks
/// the whole document's well-formedness (nesting cap and simple-value rules
/// included) and rejects extraneous trailing data — before any field is
/// examined. Then: any definite-length map is accepted, keys may come in any
/// order, unmatched integer and text keys are skipped (after an int64
/// overflow / UTF-8 check) while other key types error, a duplicate key keeps
/// the **first** value (the duplicate entry is skipped without even
/// type-checking it — fxamacker's `DupMapKeyQuiet`), a top-level or
/// field-level null/undefined decodes to the zero value, self-described-CBOR
/// and other unknown tags are unwrapped transparently (bignum tags follow the
/// big.Int paths), unassigned simple values fill the integer field, and a
/// CBOR array fills a byte-vector field element-wise. Canonicality is *not*
/// checked here — the caller's re-encode comparison is decisive. All of this
/// laxness was verified differentially against the Go implementation; see
/// `port-notes/reference.md`.
fn unmarshal(b: &[u8]) -> Result<Reference, DecodeError> {
    // Pass 1: well-formedness of the whole document, then extraneous data.
    let after = skip_item(b, 0)?;
    if !after.is_empty() {
        return Err(DecodeError::Extraneous {
            n: after.len(),
            index: b.len() - after.len(),
        });
    }
    // Pass 2: decode the record. fxamacker strips self-described-CBOR tags
    // and unwraps other tags transparently at the top level too (validating
    // built-in tag content), so a tagged map still decodes — and is then
    // rejected by the canonical-bytes comparison.
    let mut doc = strip_and_check_tags(b)?;
    loop {
        let (major, n, rest) = cbor::read_head(doc)?;
        if major != MAJOR_TAG {
            break;
        }
        if n == 2 || n == 3 {
            // A bignum can never fill the record struct.
            return Err(DecodeError::NotAMap { got: MAJOR_TAG });
        }
        doc = strip_and_check_tags(rest)?;
    }
    let (major, npairs, mut rest) = cbor::read_head(doc)?;
    if is_null_head(major, npairs, doc.len() - rest.len()) {
        // fxamacker decodes a top-level null/undefined into the zero record.
        return Ok(Reference::default());
    }
    if major != MAJOR_MAP {
        return Err(DecodeError::NotAMap { got: major });
    }
    let mut r = Reference::default();
    let mut seen = [false; 6];
    for _ in 0..npairs {
        let (kmajor, kval, after_key) = cbor::read_head(rest)?;
        if kmajor != MAJOR_UINT || kval > 5 {
            // Non-matching key. fxamacker parses integer and text-string
            // keys before matching (erroring on int64 overflow and invalid
            // UTF-8), skips unmatched ones quietly, and errors on key types
            // that can never name a struct field (byte string, array, map,
            // tag, primitives).
            match kmajor {
                MAJOR_UINT | MAJOR_NEGINT => {
                    if kval > i64::MAX as u64 {
                        return Err(DecodeError::IntKeyOverflow {
                            negative: kmajor == MAJOR_NEGINT,
                            arg: kval,
                        });
                    }
                }
                MAJOR_TSTR => {
                    if (after_key.len() as u64) < kval {
                        return Err(cbor::Error::UnexpectedEof.into());
                    }
                    if std::str::from_utf8(&after_key[..kval as usize]).is_err() {
                        return Err(DecodeError::InvalidUtf8);
                    }
                }
                _ => return Err(DecodeError::BadMapKeyType { got: kmajor }),
            }
            rest = skip_item(rest, 1)?; // the key item
            rest = skip_item(rest, 1)?; // its value
            continue;
        }
        if seen[kval as usize] {
            // Duplicate known key: first value wins, the duplicate's value
            // is skipped unexamined.
            rest = skip_item(after_key, 1)?;
            continue;
        }
        seen[kval as usize] = true;
        rest = after_key;
        match kval {
            0 => {
                let (v, rem) = read_tstr_field(rest, 0)?;
                r.name = v;
                rest = rem;
            }
            1 => {
                let (v, rem) = read_bstr_field(rest, 1)?;
                r.key = v;
                rest = rem;
            }
            2 => {
                let (v, rem) = read_tstr_field(rest, 2)?;
                r.user = v;
                rest = rem;
            }
            3 => {
                let (v, rem) = read_int64_field(rest)?;
                r.created_at = v;
                rest = rem;
            }
            4 => {
                let (v, rem) = read_bstr_field(rest, 4)?;
                r.signature = v;
                rest = rem;
            }
            _ => {
                let (v, rem) = read_bstr_field(rest, 5)?;
                r.public_key = v;
                rest = rem;
            }
        }
    }
    // No trailing check here: pass 1 already rejected extraneous data.
    let _ = rest;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{Key, Type};

    /// A valid canonical key to point references at: the key of the Blob
    /// object for "hello", matching the Go tests' `fstree.EncodeBlob`.
    fn test_key() -> Vec<u8> {
        Key::new(Type::Blob, 5, b"hello").0.to_vec()
    }

    /// The canonical CBOR encoding of Reference{Name:"n", Key:<blob "hello">,
    /// User:"u", CreatedAt:42}, pinned byte-for-byte from the Go test suite.
    const GOLDEN_HEX: &str = "a400616e0158200005ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a6702617503182a";

    fn golden_record() -> Reference {
        Reference {
            name: "n".into(),
            key: test_key(),
            user: "u".into(),
            created_at: 42,
            ..Default::default()
        }
    }

    // Port of Go TestValidateName. The "invalid utf8" row is unrepresentable
    // in a &str; see NameError::NotUtf8.
    #[test]
    fn validate_name_table() {
        let long = "x".repeat(1025);
        let ok = "y".repeat(1024);
        let cases: &[(&str, &str, bool)] = &[
            ("simple", "backup", false),
            ("with slash", "backups/2026/06", false),
            ("dotdot segment", "a/../b", false),
            ("empty segment", "a//b", false),
            ("unicode", "snapshot-éñ", false),
            ("max length", &ok, false),
            ("empty", "", true),
            ("too long", &long, true),
            ("at sign", "a@b", true),
            ("control char", "a\x01b", true),
            ("del char", "a\x7fb", true),
            ("newline", "a\nb", true),
        ];
        for &(name, ref_name, want_err) in cases {
            assert_eq!(
                validate_name(ref_name).is_err(),
                want_err,
                "{name}: validate_name({ref_name:?})"
            );
        }
    }

    #[test]
    fn validate_name_specific_errors() {
        assert_eq!(validate_name(""), Err(NameError::Empty));
        assert_eq!(validate_name(&"x".repeat(1025)), Err(NameError::TooLong));
        assert_eq!(validate_name("a@b"), Err(NameError::AtSign));
        assert_eq!(validate_name("a\x01b"), Err(NameError::ControlChar));
        assert_eq!(validate_name("a\x7fb"), Err(NameError::ControlChar));
        // Per-rune order, as in Go: '@' is reported for "@\x01", the control
        // char for "\x01@".
        assert_eq!(validate_name("@\x01"), Err(NameError::AtSign));
        assert_eq!(validate_name("\x01@"), Err(NameError::ControlChar));
        // A multi-byte name of exactly 1024 bytes passes; 1025 bytes of
        // multi-byte runes fails on the byte length.
        let e_1024 = "é".repeat(512); // 2 bytes each
        assert_eq!(e_1024.len(), 1024);
        assert_eq!(validate_name(&e_1024), Ok(()));
    }

    // Port of Go TestValidateUser (minus the invalid-utf8 row).
    #[test]
    fn validate_user_table() {
        let long = "u".repeat(1025);
        let max = "u".repeat(1024);
        let cases: &[(&str, &str, bool)] = &[
            ("simple name", "alice", false),
            ("email address", "alice@example.com", false),
            ("at sign accepted", "user@host", false),
            ("max length", &max, false),
            ("empty rejected", "", true),
            ("too long rejected", &long, true),
            ("control char rejected", "a\x01b", true),
            ("newline rejected", "a\nb", true),
            ("del char rejected", "a\x7fb", true),
        ];
        for &(name, user, want_err) in cases {
            assert_eq!(
                validate_user(user).is_err(),
                want_err,
                "{name}: validate_user({user:?})"
            );
        }
        assert_eq!(validate_user(""), Err(UserError::Empty));
        assert_eq!(validate_user(&long), Err(UserError::TooLong));
        assert_eq!(validate_user("a\nb"), Err(UserError::ControlChar));
    }

    // Port of Go TestEncodeDecodeRoundTrip.
    #[test]
    fn encode_decode_round_trip() {
        let r = Reference {
            name: "backups/home".into(),
            key: test_key(),
            user: "dragan".into(),
            created_at: 1765432100123456789,
            signature: vec![1, 2, 3],
            public_key: vec![4, 5, 6],
        };
        let b = r.encode().unwrap();
        let got = Reference::decode(&b).unwrap();
        assert_eq!(got, r);
    }

    // Port of Go TestEncodeDeterministic.
    #[test]
    fn encode_deterministic() {
        let r = golden_record();
        assert_eq!(r.encode().unwrap(), r.encode().unwrap());
    }

    // Port of Go TestSignaturePayloadExcludesSignature.
    #[test]
    fn signature_payload_excludes_signature() {
        let unsigned = golden_record();
        let mut signed = unsigned.clone();
        signed.signature = vec![9, 9, 9];
        assert_eq!(
            signed.signature_payload().unwrap(),
            unsigned.encode().unwrap()
        );
    }

    // Port of Go TestSignaturePayloadIncludesPublicKey.
    #[test]
    fn signature_payload_includes_public_key() {
        let mut with_key = golden_record();
        with_key.public_key = vec![7, 7, 7];
        let mut signed = with_key.clone();
        signed.signature = vec![9, 9, 9];

        let payload = signed.signature_payload().unwrap();
        assert_eq!(payload, with_key.encode().unwrap());

        let mut without_key = with_key.clone();
        without_key.public_key = Vec::new();
        assert_ne!(
            payload,
            without_key.encode().unwrap(),
            "signature payload does not cover the public key"
        );
    }

    // Port of Go TestEncodeRejectsInvalid, with variant checks.
    #[test]
    fn encode_rejects_invalid() {
        let bad_name = Reference {
            name: "a@b".into(),
            key: test_key(),
            ..Default::default()
        };
        assert_eq!(
            bad_name.encode().unwrap_err(),
            Error::Name(NameError::AtSign)
        );

        let bad_key = Reference {
            name: "ok".into(),
            key: vec![1, 2],
            ..Default::default()
        };
        assert_eq!(
            bad_key.encode().unwrap_err(),
            Error::Key(key::Error::BadKeyLength(2))
        );
    }

    // Port of Go TestDecodeRejectsGarbage.
    #[test]
    fn decode_rejects_garbage() {
        let err = Reference::decode(b"not cbor at all").unwrap_err();
        // 'n' = 0x6E parses as a text-string head, so this classifies as
        // "not a map", a decode-stage error — as in Go.
        assert_eq!(err, Error::Decode(DecodeError::NotAMap { got: 3 }));
        assert!(matches!(
            Reference::decode(&[]).unwrap_err(),
            Error::Decode(DecodeError::Cbor(cbor::Error::UnexpectedEof))
        ));
    }

    // Port of Go TestGoldenVector: pins the wire format.
    #[test]
    fn golden_vector() {
        let b = golden_record().encode().unwrap();
        assert_eq!(hex::encode(&b), GOLDEN_HEX);
        // And it round-trips.
        assert_eq!(Reference::decode(&b).unwrap(), golden_record());
    }

    // Port of Go TestDecodeRejectsExtraMapKey: golden bytes with the map head
    // changed a4 -> a5 and an extra pair (09 = int 9, 61 78 = text "x")
    // appended. The lax unmarshal skips key 9, so the rejection comes from
    // the canonical-bytes comparison — same as Go.
    #[test]
    fn decode_rejects_extra_map_key() {
        let extra = hex::decode(
            "a500616e0158200005ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a6702617503182a096178",
        )
        .unwrap();
        assert_eq!(Reference::decode(&extra).unwrap_err(), Error::NotCanonical);
    }

    // Port of Go TestEncodeRejectsTooLongUserAndSignature.
    #[test]
    fn encode_rejects_too_long_user_signature_public_key() {
        let k = test_key();
        let base = |user: &str, sig: usize, pk: usize| Reference {
            name: "n".into(),
            key: k.clone(),
            user: user.into(),
            signature: vec![0; sig],
            public_key: vec![0; pk],
            ..Default::default()
        };

        // At the limit: accepted.
        base(&"u".repeat(MAX_USER_LEN), 0, 0).encode().unwrap();
        base("", MAX_SIGNATURE_LEN, 0).encode().unwrap();
        base("", 0, MAX_PUBLIC_KEY_LEN).encode().unwrap();

        // One byte over: rejected.
        assert_eq!(
            base(&"u".repeat(MAX_USER_LEN + 1), 0, 0)
                .encode()
                .unwrap_err(),
            Error::User(UserError::TooLong)
        );
        assert_eq!(
            base("", MAX_SIGNATURE_LEN + 1, 0).encode().unwrap_err(),
            Error::SignatureTooLong
        );
        assert_eq!(
            base("", 0, MAX_PUBLIC_KEY_LEN + 1).encode().unwrap_err(),
            Error::PublicKeyTooLong
        );
    }

    // Port of Go TestDecodeRejectsNonCanonicalEncoding: 18 2a (uint 42)
    // replaced by 19 00 2a — valid CBOR, not minimal. The lax unmarshal
    // accepts it; the re-encode comparison rejects it, as in Go.
    #[test]
    fn decode_rejects_non_canonical_encoding() {
        let non_canon = hex::decode(
            "a400616e0158200005ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a670261750319002a",
        )
        .unwrap();
        assert_eq!(
            Reference::decode(&non_canon).unwrap_err(),
            Error::NotCanonical
        );
    }

    // Decode classification: a canonical encoding of an *invalid* record is
    // "invalid reference: ...", not a canonicality error.
    #[test]
    fn decode_invalid_record_is_classified_invalid() {
        let bad = Reference {
            name: "a@b".into(),
            key: test_key(),
            ..Default::default()
        };
        let b = bad.encode_unchecked();
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Invalid(Box::new(Error::Name(NameError::AtSign)))
        );
        // Validation order inside decode: name before key.
        let both_bad = Reference {
            name: "".into(),
            key: vec![1],
            ..Default::default()
        };
        assert_eq!(
            Reference::decode(&both_bad.encode_unchecked()).unwrap_err(),
            Error::Invalid(Box::new(Error::Name(NameError::Empty)))
        );
    }

    #[test]
    fn validate_order_matches_go() {
        // name -> key -> user -> signature -> public key.
        let mut r = Reference {
            name: "".into(),
            key: vec![1],
            user: "a\x01".into(),
            created_at: 0,
            signature: vec![0; MAX_SIGNATURE_LEN + 1],
            public_key: vec![0; MAX_PUBLIC_KEY_LEN + 1],
        };
        assert_eq!(r.encode().unwrap_err(), Error::Name(NameError::Empty));
        r.name = "n".into();
        assert_eq!(
            r.encode().unwrap_err(),
            Error::Key(key::Error::BadKeyLength(1))
        );
        r.key = test_key();
        assert_eq!(r.encode().unwrap_err(), Error::User(UserError::ControlChar));
        r.user = String::new(); // empty user is valid at the record level
        assert_eq!(r.encode().unwrap_err(), Error::SignatureTooLong);
        r.signature = Vec::new();
        assert_eq!(r.encode().unwrap_err(), Error::PublicKeyTooLong);
        r.public_key = Vec::new();
        r.encode().unwrap();
    }

    // Duplicate map keys survive the lax unmarshal (first value wins, the
    // duplicate is skipped) but the re-encode comparison rejects the bytes —
    // as in Go.
    #[test]
    fn decode_rejects_duplicate_keys() {
        let mut b = golden_record().encode_unchecked();
        b[0] = 0xa5; // 5 pairs
        b.push(0x03); // key 3 again
        b.push(0x07); // created_at 7
        assert_eq!(Reference::decode(&b).unwrap_err(), Error::NotCanonical);
    }

    // fxamacker's DupMapKeyQuiet semantics, verified differentially against
    // Go: the first value wins, and the duplicate entry's value is skipped
    // without type-checking — a type-mismatched duplicate value is NOT a
    // decode error.
    #[test]
    fn decode_duplicate_key_first_wins_and_skips_unchecked() {
        // {0: "n", 0: 5}: the uint 5 would be a WrongFieldType for key 0,
        // but as a duplicate it is skipped; the record then fails validation
        // on its missing key — exactly Go's classification.
        let b = [0xa2, 0x00, 0x61, b'n', 0x00, 0x05];
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Invalid(Box::new(Error::Key(key::Error::BadKeyLength(0))))
        );
        // Duplicate with a container value is skipped wholesale.
        let mut b = golden_record().encode_unchecked();
        b[0] = 0xa5;
        b.extend_from_slice(&[0x02, 0xa1, 0x01, 0x02]); // 2: {1: 2}
        assert_eq!(Reference::decode(&b).unwrap_err(), Error::NotCanonical);
    }

    // A top-level null/undefined decodes to the zero record (fxamacker),
    // which then fails validation — Go classifies this as "invalid".
    #[test]
    fn decode_top_level_null_is_zero_record() {
        for b in [[0xf6], [0xf7]] {
            assert_eq!(
                Reference::decode(&b).unwrap_err(),
                Error::Invalid(Box::new(Error::Name(NameError::Empty))),
                "input {b:02x?}"
            );
        }
        // Trailing bytes after the null are still extraneous.
        assert_eq!(
            Reference::decode(&[0xf6, 0x01]).unwrap_err(),
            Error::Decode(DecodeError::Extraneous { n: 1, index: 1 })
        );
    }

    // Unknown keys of non-integer type are skipped by the lax unmarshal, then
    // rejected by the canonical comparison (fxamacker behaves the same).
    #[test]
    fn decode_rejects_extra_text_key() {
        let mut b = golden_record().encode_unchecked();
        b[0] = 0xa5;
        b.extend_from_slice(&[0x61, b'x', 0x05]); // "x": 5
        assert_eq!(Reference::decode(&b).unwrap_err(), Error::NotCanonical);
        // Unknown key with a nested container value.
        let mut b = golden_record().encode_unchecked();
        b[0] = 0xa5;
        b.extend_from_slice(&[0x08, 0x82, 0x01, 0xa1, 0x01, 0x02]); // 8: [1, {1:2}]
        assert_eq!(Reference::decode(&b).unwrap_err(), Error::NotCanonical);
    }

    // Map-key type handling, verified against the Go implementation:
    // integer and text keys that match no field are skipped quietly; keys of
    // any other type are an unmarshal error.
    #[test]
    fn decode_map_key_types() {
        // {-1: 1}: negint key skipped, zero record fails validation.
        assert_eq!(
            Reference::decode(&[0xa1, 0x20, 0x01]).unwrap_err(),
            Error::Invalid(Box::new(Error::Name(NameError::Empty)))
        );
        // {-1: 1} with a trailing byte is extraneous data.
        assert_eq!(
            Reference::decode(&[0xa1, 0x20, 0x01, 0x01]).unwrap_err(),
            Error::Decode(DecodeError::Extraneous { n: 1, index: 3 })
        );
        // {"x": 5} and {"0": 5}: text keys skipped (keyasint fields match
        // integer keys only), zero record fails validation.
        for b in [[0xa1, 0x61, b'x', 0x05], [0xa1, 0x61, b'0', 0x05]] {
            assert_eq!(
                Reference::decode(&b).unwrap_err(),
                Error::Invalid(Box::new(Error::Name(NameError::Empty))),
                "input {b:02x?}"
            );
        }
        // Byte-string, array, map, tag, and primitive keys error.
        let cases: &[(&[u8], u8)] = &[
            (&[0xa1, 0x44, 1, 2, 3, 4, 0x01], 2),                   // bstr
            (&[0xa1, 0x80, 0x01], 4),                               // array
            (&[0xa1, 0xa0, 0x01], 5),                               // map
            (&[0xa1, 0xc0, 0x61, b'a', 0x01], 6),                   // tag
            (&[0xa1, 0xf4, 0x01], 7),                               // false
            (&[0xa1, 0xf6, 0x01], 7),                               // null
            (&[0xa1, 0xfb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0, 0x01], 7), // float
        ];
        for &(b, got) in cases {
            assert_eq!(
                Reference::decode(b).unwrap_err(),
                Error::Decode(DecodeError::BadMapKeyType { got }),
                "input {b:02x?}"
            );
        }
    }

    /// The golden encoding with one hex substring substituted (helper for
    /// exotic-but-decodable rewrites).
    fn golden_with(old: &str, new: &str) -> Vec<u8> {
        let g = hex::encode(golden_record().encode_unchecked());
        let replaced = g.replacen(old, new, 1);
        assert_ne!(g, replaced, "pattern {old} not found in golden bytes");
        hex::decode(replaced).unwrap()
    }

    // fxamacker's well-formedness pass rejects two-byte simple values < 32
    // (RFC 8949 §3.3) anywhere in the document, before any field decoding —
    // verified against Go: {9: simple(16)} is a decode error there too, not
    // an invalid zero record.
    #[test]
    fn decode_rejects_low_two_byte_simple_values() {
        assert_eq!(
            Reference::decode(&[0xa1, 0x09, 0xf8, 0x10]).unwrap_err(),
            Error::Decode(DecodeError::InvalidSimpleValue(16))
        );
        assert_eq!(
            Reference::decode(&[0xf8, 0x00]).unwrap_err(),
            Error::Decode(DecodeError::InvalidSimpleValue(0))
        );
        // Boundary: 31 is rejected, 32 is a well-formed simple value.
        assert_eq!(
            Reference::decode(&[0xf8, 0x1f]).unwrap_err(),
            Error::Decode(DecodeError::InvalidSimpleValue(31))
        );
        assert_eq!(
            Reference::decode(&[0xa1, 0x03, 0xf8, 0x20]).unwrap_err(),
            Error::Invalid(Box::new(Error::Name(NameError::Empty)))
        );
    }

    // fxamacker fills unassigned simple values into integer fields as their
    // numeric value (fillPositiveInt), so the record decodes and the
    // rejection is the canonical comparison; bools and floats stay errors.
    #[test]
    fn decode_simple_values_fill_created_at() {
        // 18 2a (uint 42) -> f8 2a (simple value 42): same decoded record.
        assert_eq!(
            Reference::decode(&golden_with("03182a", "03f82a")).unwrap_err(),
            Error::NotCanonical
        );
        assert_eq!(
            Reference::decode(&golden_with("03182a", "03e5")).unwrap_err(),
            Error::NotCanonical
        );
        assert_eq!(
            Reference::decode(&golden_with("03182a", "03f4")).unwrap_err(),
            Error::Decode(DecodeError::WrongFieldType { field: 3, got: 7 })
        );
        // Simple values never fill string or byte-vector fields.
        assert_eq!(
            Reference::decode(&[0xa1, 0x00, 0xf0]).unwrap_err(),
            Error::Decode(DecodeError::WrongFieldType { field: 0, got: 7 })
        );
    }

    // fxamacker parses integer map keys as int64 before matching and errors
    // on overflow; in-range unmatched keys are still skipped quietly.
    #[test]
    fn decode_int_key_overflow() {
        let key_case = |head: &[u8]| {
            let mut b = vec![0xa1];
            b.extend_from_slice(head);
            b.push(0x01);
            Reference::decode(&b).unwrap_err()
        };
        let max = &(i64::MAX as u64).to_be_bytes();
        let over = &(1u64 << 63).to_be_bytes();
        let mut h = vec![0x1b];
        h.extend_from_slice(over);
        assert_eq!(
            key_case(&h),
            Error::Decode(DecodeError::IntKeyOverflow {
                negative: false,
                arg: 1 << 63,
            })
        );
        h = vec![0x3b];
        h.extend_from_slice(over);
        assert_eq!(
            key_case(&h),
            Error::Decode(DecodeError::IntKeyOverflow {
                negative: true,
                arg: 1 << 63,
            })
        );
        // At the boundary both signs still fit int64 and are skipped quietly.
        for first in [0x1b, 0x3b] {
            h = vec![first];
            h.extend_from_slice(max);
            assert_eq!(
                key_case(&h),
                Error::Invalid(Box::new(Error::Name(NameError::Empty)))
            );
        }
    }

    // fxamacker parses text-string map keys (with the UTF-8 check) before
    // discovering they match no field.
    #[test]
    fn decode_text_key_utf8_checked() {
        assert_eq!(
            Reference::decode(&[0xa1, 0x62, 0xff, 0xfe, 0x05]).unwrap_err(),
            Error::Decode(DecodeError::InvalidUtf8)
        );
    }

    // fxamacker strips self-described-CBOR tags (55799) and unwraps unknown
    // tags transparently, at the top level and around field values; the
    // decoded record is then rejected by the canonical comparison. Built-in
    // tags 0-3 have their content type validated; bignum tags 2/3 follow the
    // big.Int paths. All verified against Go.
    #[test]
    fn decode_tag_handling() {
        let gold = golden_record().encode_unchecked();
        let wrap = |tag: &[u8]| {
            let mut b = tag.to_vec();
            b.extend_from_slice(&gold);
            Reference::decode(&b).unwrap_err()
        };
        // Tagged top-level map still decodes, then fails canonicality.
        assert_eq!(wrap(&[0xd9, 0xd9, 0xf7]), Error::NotCanonical);
        assert_eq!(wrap(&[0xc4]), Error::NotCanonical);
        // Built-in tag 0 requires text-string content: a tagged map errors.
        assert_eq!(
            wrap(&[0xc0]),
            Error::Decode(DecodeError::BadTagContent { tag: 0, got: 5 })
        );
        // A top-level bignum can never fill the record.
        assert_eq!(
            Reference::decode(&[0xc2, 0x41, 0x05]).unwrap_err(),
            Error::Decode(DecodeError::NotAMap { got: 6 })
        );
        // Tagged field values decode transparently -> non-canonical.
        assert_eq!(
            Reference::decode(&golden_with("00616e", "00d9d9f7616e")).unwrap_err(),
            Error::NotCanonical
        );
        assert_eq!(
            Reference::decode(&golden_with("00616e", "00c4616e")).unwrap_err(),
            Error::NotCanonical
        );
        // created_at via epoch tag 1 and bignum tag 2 (value 42).
        assert_eq!(
            Reference::decode(&golden_with("03182a", "03c1182a")).unwrap_err(),
            Error::NotCanonical
        );
        assert_eq!(
            Reference::decode(&golden_with("03182a", "03c2412a")).unwrap_err(),
            Error::NotCanonical
        );
        // The key field filled from a bignum's content bytes.
        assert_eq!(
            Reference::decode(&golden_with("015820", "01c25820")).unwrap_err(),
            Error::NotCanonical
        );
        // Built-in content-type violations are decode errors.
        assert_eq!(
            Reference::decode(&[0xa1, 0x03, 0xc0, 0x05]).unwrap_err(),
            Error::Decode(DecodeError::BadTagContent { tag: 0, got: 0 })
        );
        assert_eq!(
            Reference::decode(&[0xa1, 0x03, 0xc2, 0x05]).unwrap_err(),
            Error::Decode(DecodeError::BadTagContent { tag: 2, got: 0 })
        );
        // Bignums never fill strings.
        assert_eq!(
            Reference::decode(&[0xa1, 0x00, 0xc2, 0x41, 0x05]).unwrap_err(),
            Error::Decode(DecodeError::WrongFieldType { field: 0, got: 6 })
        );
        // Bignum overflow boundaries on created_at.
        let bignum = |tag: u8, content: &[u8]| {
            let mut b = vec![0xa1, 0x03, tag, 0x40 + content.len() as u8];
            b.extend_from_slice(content);
            Reference::decode(&b).unwrap_err()
        };
        assert_eq!(
            bignum(0xc2, &(1u64 << 63).to_be_bytes()),
            Error::Decode(DecodeError::BignumOverflow { negative: false })
        );
        assert_eq!(
            bignum(0xc3, &(1u64 << 63).to_be_bytes()),
            Error::Decode(DecodeError::BignumOverflow { negative: true })
        );
        // Magnitude i64::MAX fits both ways (leading zeros insignificant);
        // the single-field record then fails validation, not decoding.
        let mut nine = vec![0u8];
        nine.extend_from_slice(&(i64::MAX as u64).to_be_bytes());
        assert_eq!(
            bignum(0xc3, &nine),
            Error::Invalid(Box::new(Error::Name(NameError::Empty)))
        );
    }

    // fxamacker decodes CBOR arrays into []byte fields element-wise (like
    // encoding/json); in-range elements produce a record that then fails the
    // canonical comparison, out-of-range or mistyped elements are decode
    // errors. Verified against Go.
    #[test]
    fn decode_arrays_fill_byte_fields() {
        // The golden key rewritten as an array of 32 integers.
        let mut arr = String::from("019820");
        for byte in test_key() {
            if byte < 24 {
                arr.push_str(&hex::encode([byte]));
            } else {
                arr.push_str(&hex::encode([0x18, byte]));
            }
        }
        let key_hex = format!("015820{}", hex::encode(test_key()));
        assert_eq!(
            Reference::decode(&golden_with(&key_hex, &arr)).unwrap_err(),
            Error::NotCanonical
        );
        let field1 = |value: &[u8]| {
            let mut b = vec![0xa1, 0x01];
            b.extend_from_slice(value);
            Reference::decode(&b).unwrap_err()
        };
        // Element out of byte range.
        assert_eq!(
            field1(&[0x81, 0x19, 0x01, 0x2c]),
            Error::Decode(DecodeError::ByteElemOverflow { field: 1 })
        );
        // Negative, text, and nested-array elements are type errors.
        assert_eq!(
            field1(&[0x81, 0x20]),
            Error::Decode(DecodeError::ByteElemType { field: 1, got: 1 })
        );
        assert_eq!(
            field1(&[0x81, 0x61, b'x']),
            Error::Decode(DecodeError::ByteElemType { field: 1, got: 3 })
        );
        assert_eq!(
            field1(&[0x81, 0x81, 0x01]),
            Error::Decode(DecodeError::ByteElemType { field: 1, got: 4 })
        );
        // Null elements become zero bytes; bignum elements follow big.Int
        // rules (tag 2 in range fills, tag 3 never fits a byte). A name is
        // included so validation reaches the decoded key.
        let named_field1 = |value: &[u8]| {
            let mut b = vec![0xa2, 0x00, 0x61, b'n', 0x01];
            b.extend_from_slice(value);
            Reference::decode(&b).unwrap_err()
        };
        assert_eq!(
            named_field1(&[0x82, 0xf6, 0x01]),
            Error::Invalid(Box::new(Error::Key(key::Error::BadKeyLength(2))))
        );
        assert_eq!(
            named_field1(&[0x81, 0xc2, 0x41, 0x05]),
            Error::Invalid(Box::new(Error::Key(key::Error::BadKeyLength(1))))
        );
        assert_eq!(
            field1(&[0x81, 0xc2, 0x42, 0x01, 0x01]),
            Error::Decode(DecodeError::ByteElemOverflow { field: 1 })
        );
        assert_eq!(
            field1(&[0x81, 0xc3, 0x41, 0x05]),
            Error::Decode(DecodeError::ByteElemType { field: 1, got: 6 })
        );
    }

    // The new decode-stage diagnostics that reproduce fxamacker verbatim.
    #[test]
    fn fxamacker_verbatim_messages() {
        assert_eq!(
            DecodeError::InvalidSimpleValue(16).to_string(),
            "cbor: invalid simple value 16 for type primitives"
        );
        assert_eq!(
            DecodeError::IntKeyOverflow {
                negative: false,
                arg: u64::MAX,
            }
            .to_string(),
            "cbor: cannot unmarshal positive integer into Go value of type int64 \
             (18446744073709551615 overflows Go's int64)"
        );
        assert_eq!(
            DecodeError::IntKeyOverflow {
                negative: true,
                arg: 1 << 63,
            }
            .to_string(),
            "cbor: cannot unmarshal negative integer into Go value of type int64 \
             (-1-9223372036854775808 overflows Go's int64)"
        );
        assert_eq!(
            DecodeError::BadTagContent { tag: 0, got: 0 }.to_string(),
            "cbor: tag number 0 must be followed by text string, got positive integer"
        );
        assert_eq!(
            DecodeError::BadTagContent { tag: 1, got: 5 }.to_string(),
            "cbor: tag number 1 must be followed by integer or floating-point number, got map"
        );
        assert_eq!(
            DecodeError::BadTagContent { tag: 3, got: 6 }.to_string(),
            "cbor: tag number 2 or 3 must be followed by byte string, got tag"
        );
    }

    // Indefinite-length items: Go's fxamacker decodes them and rejects at the
    // canonical comparison; this port rejects them one step earlier, at the
    // head reader. Rejection either way (see port-notes/reference.md).
    #[test]
    fn decode_rejects_indefinite_length_map() {
        // {_ 0: "n"} in indefinite encoding, then break.
        let b = [0xbf, 0x00, 0x61, b'n', 0xff];
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::Cbor(cbor::Error::UnsupportedAdditionalInfo(
                31
            )))
        );
    }

    // Hostile nesting must fail cleanly, not overflow the stack, and the
    // boundaries match fxamacker exactly (verified against Go): under the
    // top-level map, 31 nested arrays pass well-formedness and 32 fail; 32
    // nested tags pass and 33 fail (the first tag of a chain adds no level).
    #[test]
    fn decode_nesting_boundaries() {
        let nested = |item: u8, n: usize| {
            let mut b = vec![0xa1, 0x09]; // {9: ...}
            b.extend(std::iter::repeat_n(item, n));
            b.push(0x01);
            b
        };
        assert_eq!(
            Reference::decode(&nested(0x81, 100)).unwrap_err(),
            Error::Decode(DecodeError::MaxNestedLevels)
        );
        // Arrays: 31 well-formed (fails later on validation), 32 not.
        assert_eq!(
            Reference::decode(&nested(0x81, 31)).unwrap_err(),
            Error::Invalid(Box::new(Error::Name(NameError::Empty)))
        );
        assert_eq!(
            Reference::decode(&nested(0x81, 32)).unwrap_err(),
            Error::Decode(DecodeError::MaxNestedLevels)
        );
        // Tags: 32 well-formed, 33 not.
        assert_eq!(
            Reference::decode(&nested(0xc0, 32)).unwrap_err(),
            Error::Invalid(Box::new(Error::Name(NameError::Empty)))
        );
        assert_eq!(
            Reference::decode(&nested(0xc0, 33)).unwrap_err(),
            Error::Decode(DecodeError::MaxNestedLevels)
        );
        // Deep tag chains must not overflow the stack either.
        assert_eq!(
            Reference::decode(&nested(0xc0, 1_000_000)).unwrap_err(),
            Error::Decode(DecodeError::MaxNestedLevels)
        );
    }

    // The well-formedness pass runs before field decoding, as in fxamacker:
    // a nesting violation beats a field-type error, and extraneous trailing
    // data beats everything (verified against Go).
    #[test]
    fn decode_well_formedness_precedes_field_decoding() {
        // {4: 31 nested arrays}: well-formed, so field decoding fires — the
        // outer array starts an element-wise []byte decode whose first
        // element is another array (fxamacker: array into uint8 errors).
        let deep_field = |n: usize| {
            let mut b = vec![0xa1, 0x04];
            b.extend(std::iter::repeat_n(0x81, n));
            b.push(0x01);
            b
        };
        assert_eq!(
            Reference::decode(&deep_field(31)).unwrap_err(),
            Error::Decode(DecodeError::ByteElemType { field: 4, got: 4 })
        );
        // {4: 32 nested arrays}: rejected in the well-formedness pass.
        assert_eq!(
            Reference::decode(&deep_field(32)).unwrap_err(),
            Error::Decode(DecodeError::MaxNestedLevels)
        );
        // {0: 5} + trailing byte: extraneous data, not a field-type error.
        assert_eq!(
            Reference::decode(&[0xa1, 0x00, 0x05, 0x01]).unwrap_err(),
            Error::Decode(DecodeError::Extraneous { n: 1, index: 3 })
        );
        // Bare non-map + trailing byte: extraneous data, not "not a map".
        assert_eq!(
            Reference::decode(&[0x05, 0x05]).unwrap_err(),
            Error::Decode(DecodeError::Extraneous { n: 1, index: 1 })
        );
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut b = golden_record().encode_unchecked();
        let index = b.len();
        b.extend_from_slice(&[0x00, 0x01]);
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::Extraneous { n: 2, index })
        );
    }

    #[test]
    fn decode_rejects_wrong_field_types() {
        // Key 0 (name) as a uint.
        let b = [0xa1, 0x00, 0x05];
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::WrongFieldType { field: 0, got: 0 })
        );
        // Key 1 (key) as a text string.
        let b = [0xa1, 0x01, 0x61, b'x'];
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::WrongFieldType { field: 1, got: 3 })
        );
        // Key 3 (created_at) as a float64.
        let b = [0xa1, 0x03, 0xfb, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::WrongFieldType { field: 3, got: 7 })
        );
    }

    // Null (and undefined) values decode to the zero value, as fxamacker
    // does; the record is then rejected downstream (validation or
    // canonicality), matching Go's classification.
    #[test]
    fn decode_null_values_zero_then_reject() {
        // {0: null}: name becomes "", validation rejects it.
        let b = [0xa1, 0x00, 0xf6];
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Invalid(Box::new(Error::Name(NameError::Empty)))
        );
        // Valid record plus {4: null}: signature stays absent, so the bytes
        // are non-canonical.
        let mut b = golden_record().encode_unchecked();
        b[0] = 0xa5;
        b.extend_from_slice(&[0x04, 0xf6]);
        assert_eq!(Reference::decode(&b).unwrap_err(), Error::NotCanonical);
    }

    #[test]
    fn decode_rejects_invalid_utf8_text() {
        // {0: tstr(2) ff fe ...}: CBOR text strings must be UTF-8; Go's
        // decoder rejects this during unmarshalling too.
        let b = [0xa1, 0x00, 0x62, 0xff, 0xfe];
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::InvalidUtf8)
        );
    }

    #[test]
    fn decode_created_at_overflow() {
        // {3: uint 2^63}: overflows int64.
        let mut b = vec![0xa1, 0x03, 0x1b];
        b.extend_from_slice(&(1u64 << 63).to_be_bytes());
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::IntOverflow { negative: false })
        );
        // {3: negint with argument 2^63}: value -1 - 2^63 underflows int64.
        let mut b = vec![0xa1, 0x03, 0x3b];
        b.extend_from_slice(&(1u64 << 63).to_be_bytes());
        assert_eq!(
            Reference::decode(&b).unwrap_err(),
            Error::Decode(DecodeError::IntOverflow { negative: true })
        );
    }

    #[test]
    fn created_at_extremes_round_trip() {
        for created_at in [i64::MIN, -1, 0, 1, i64::MAX] {
            let mut r = golden_record();
            r.created_at = created_at;
            let b = r.encode().unwrap();
            assert_eq!(Reference::decode(&b).unwrap(), r, "created_at {created_at}");
        }
    }

    #[test]
    fn negative_created_at_encoding() {
        let mut r = golden_record();
        r.created_at = -1;
        let b = r.encode().unwrap();
        // ... 03 20: key 3, negint -1 in shortest form.
        assert_eq!(&b[b.len() - 2..], &[0x03, 0x20]);
        // i64::MIN encodes as major-1 argument 2^63 - 1.
        r.created_at = i64::MIN;
        let b = r.encode().unwrap();
        let mut want = vec![0x03, 0x3b];
        want.extend_from_slice(&(i64::MAX as u64).to_be_bytes());
        assert_eq!(&b[b.len() - 10..], &want[..]);
    }

    #[test]
    fn empty_signature_means_absent() {
        // Empty signature/public_key: 4-pair map, keys 4 and 5 omitted.
        let b = golden_record().encode().unwrap();
        assert_eq!(b[0], 0xa4);
        // Non-empty signature only: 5 pairs.
        let mut r = golden_record();
        r.signature = vec![1];
        let b = r.encode().unwrap();
        assert_eq!(b[0], 0xa5);
        assert_eq!(&b[b.len() - 3..], &[0x04, 0x41, 0x01]);
        // Both: 6 pairs.
        r.public_key = vec![2];
        let b = r.encode().unwrap();
        assert_eq!(b[0], 0xa6);
        assert_eq!(&b[b.len() - 3..], &[0x05, 0x41, 0x02]);
    }

    #[test]
    fn decode_accepts_out_of_order_keys_only_via_canonical_check() {
        // Keys in reverse order: unmarshal accepts, canonical compare rejects.
        let mut b = Vec::new();
        cbor::append_head(&mut b, MAJOR_MAP, 4);
        cbor::append_head(&mut b, MAJOR_UINT, 3);
        b.push(0x18);
        b.push(42);
        cbor::append_head(&mut b, MAJOR_UINT, 2);
        cbor::append_tstr(&mut b, "u");
        cbor::append_head(&mut b, MAJOR_UINT, 1);
        cbor::append_bstr(&mut b, &test_key());
        cbor::append_head(&mut b, MAJOR_UINT, 0);
        cbor::append_tstr(&mut b, "n");
        assert_eq!(Reference::decode(&b).unwrap_err(), Error::NotCanonical);
    }

    // Error message texts pin the Go diagnostics.
    #[test]
    fn error_messages_match_go() {
        assert_eq!(
            NameError::Empty.to_string(),
            "reference name must not be empty"
        );
        assert_eq!(
            NameError::TooLong.to_string(),
            "reference name exceeds 1024 bytes"
        );
        assert_eq!(
            NameError::NotUtf8.to_string(),
            "reference name must be valid UTF-8"
        );
        assert_eq!(
            NameError::AtSign.to_string(),
            "reference name must not contain '@'"
        );
        assert_eq!(
            NameError::ControlChar.to_string(),
            "reference name must not contain control characters"
        );
        assert_eq!(UserError::Empty.to_string(), "user must not be empty");
        assert_eq!(UserError::TooLong.to_string(), "user exceeds 1024 bytes");
        assert_eq!(UserError::NotUtf8.to_string(), "user must be valid UTF-8");
        assert_eq!(
            UserError::ControlChar.to_string(),
            "user must not contain control characters"
        );
        assert_eq!(
            Error::Name(NameError::Empty).to_string(),
            "reference name must not be empty",
            "name errors are returned unwrapped, as in Go"
        );
        assert_eq!(
            Error::Key(key::Error::BadKeyLength(2)).to_string(),
            "reference key: key: data is not 32 bytes: got 2"
        );
        assert_eq!(
            Error::User(UserError::ControlChar).to_string(),
            "reference user: user must not contain control characters"
        );
        assert_eq!(
            Error::SignatureTooLong.to_string(),
            "reference signature exceeds 65536 bytes"
        );
        assert_eq!(
            Error::PublicKeyTooLong.to_string(),
            "reference public key exceeds 16384 bytes"
        );
        assert_eq!(
            Error::NotCanonical.to_string(),
            "reference encoding is not canonical"
        );
        assert_eq!(
            Error::Invalid(Box::new(Error::Name(NameError::Empty))).to_string(),
            "invalid reference: reference name must not be empty"
        );
        assert!(
            Error::Decode(DecodeError::NotAMap { got: 4 })
                .to_string()
                .starts_with("decoding reference: cbor: cannot unmarshal array"),
        );
        // Wrapped errors expose their source.
        let e = Error::Invalid(Box::new(Error::Name(NameError::Empty)));
        let src = std::error::Error::source(&e).unwrap();
        assert_eq!(src.to_string(), "reference name must not be empty");
    }

    #[test]
    fn signature_payload_validates() {
        // signature_payload encodes, so it validates: an invalid record errors.
        let r = Reference {
            name: "a@b".into(),
            key: test_key(),
            signature: vec![9],
            ..Default::default()
        };
        assert_eq!(
            r.signature_payload().unwrap_err(),
            Error::Name(NameError::AtSign)
        );
    }
}
