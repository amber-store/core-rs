//! The Commit object (CAS type 5): the analogue of a git commit — a directory
//! root, ordered parent commits, author, committer and message, with an
//! optional opaque signature (Go package `commit`).
//!
//! Encoding is RFC 8949 §4.2 core-deterministic CBOR (canonical map, integer
//! keys), the fstree and reference convention. See `architecture/commits.md`.

use std::collections::HashSet;

use crate::cbor::{self, MAJOR_ARRAY, MAJOR_MAP, MAJOR_UINT};
use crate::fstree::{self, CborError};
use crate::key::{self, Key, Type};

/// The maximum number of parent commits (Go: `MaxParents`).
pub const MAX_PARENTS: usize = 256;

/// The maximum byte length of an identity's name or email (Go:
/// `MaxIdentityLen`).
pub const MAX_IDENTITY_LEN: usize = 1024;

/// The maximum message length in bytes, 1 MiB (Go: `MaxMessageLen`).
pub const MAX_MESSAGE_LEN: usize = 1 << 20;

/// The maximum `signature` length in bytes, 64 KiB (Go: `MaxSignatureLen`).
pub const MAX_SIGNATURE_LEN: usize = 64 << 10;

/// The maximum `public_key` length in bytes, 16 KiB (Go: `MaxPublicKeyLen`).
pub const MAX_PUBLIC_KEY_LEN: usize = 16 << 10;

/// Bounds [`Identity::tz_offset`] to under a day either side of UTC (Go:
/// `MaxTZOffset`).
pub const MAX_TZ_OFFSET: i32 = 1439;

/// A violation of the identity-string rules (Go: `validateText`). `Display`
/// reproduces the Go diagnostic, which the caller prefixes with the field
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The string exceeds [`MAX_IDENTITY_LEN`] bytes.
    #[error("exceeds {} bytes", MAX_IDENTITY_LEN)]
    TooLong,
    /// The string is not valid UTF-8. Unreachable here (`String` is UTF-8 by
    /// construction — Go strings are not); kept so the Go rule and its
    /// diagnostic stay documented. On the decode path invalid UTF-8 is
    /// rejected during unmarshalling, where Go rejects it too.
    #[error("must be valid UTF-8")]
    NotUtf8,
    /// The string contains a control character (< 0x20 or 0x7F).
    #[error("must not contain control characters")]
    ControlChar,
}

/// An [`Identity`] rule violation (Go: `Identity.validate`, and the tz range
/// check of `wireIdentity.identity`). `Display` reproduces the Go
/// diagnostics verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The name is empty.
    #[error("name must not be empty")]
    NameEmpty,
    /// The name broke a string rule.
    #[error("name {0}")]
    Name(#[source] TextError),
    /// The email broke a string rule.
    #[error("email {0}")]
    Email(#[source] TextError),
    /// The tz offset is outside ±[`MAX_TZ_OFFSET`] minutes; carries the
    /// offending value (an `i64`, because the wire integer may exceed `i32`).
    #[error("tz offset {0} outside ±{max} minutes", max = MAX_TZ_OFFSET)]
    TzOffset(i64),
}

/// A failure converting the unmarshalled wire shape into a [`Commit`] (Go:
/// `wireCommit.commit`). Only [`Commit::decode`] produces these, wrapped in
/// [`Error::Convert`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// Key 0 is not a canonical 32-byte key.
    #[error("tree: {0}")]
    Tree(#[source] key::Error),
    /// An element of key 1 is not a canonical 32-byte key.
    #[error("parent {index}: {source}")]
    Parent {
        /// The parent's position.
        index: usize,
        /// The key parse failure.
        source: key::Error,
    },
    /// The author's tz offset is out of range.
    #[error("author: {0}")]
    Author(#[source] IdentityError),
    /// The committer's tz offset is out of range.
    #[error("committer: {0}")]
    Committer(#[source] IdentityError),
}

/// Errors from encoding, decoding, and validating commits.
///
/// The validation variants are returned bare from [`Commit::encode`], exactly
/// as Go's `Encode` returns `validate()`'s error; [`Commit::decode`] wraps the
/// same failures in [`Error::Invalid`], mirroring Go's `"invalid commit: %w"`.
/// `Display` messages reproduce the Go diagnostics verbatim, and decode-stage
/// messages reproduce fxamacker/cbor's (see `fstree::CborError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The tree is not a canonical key.
    #[error("commit tree: {0}")]
    Tree(#[source] key::Error),
    /// The tree is not a `DirLeaf` or `DirNode` key.
    #[error("commit tree {key} is not a directory key (type {type_})")]
    TreeNotDirectory {
        /// The offending tree key.
        key: Key,
        /// Its object type.
        type_: Type,
    },
    /// More than [`MAX_PARENTS`] parents; carries the count.
    #[error("commit has {0} parents, more than {max}", max = MAX_PARENTS)]
    TooManyParents(usize),
    /// A parent is not a canonical key.
    #[error("commit parent {index}: {source}")]
    Parent {
        /// The parent's position.
        index: usize,
        /// The key validation failure.
        source: key::Error,
    },
    /// A parent is not a `Commit` key.
    #[error("commit parent {index}: {key} is not a commit key (type {type_})")]
    ParentNotCommit {
        /// The parent's position.
        index: usize,
        /// The offending parent key.
        key: Key,
        /// Its object type.
        type_: Type,
    },
    /// A parent repeats an earlier one.
    #[error("commit parent {index}: duplicate {key}")]
    DuplicateParent {
        /// The position of the repeat.
        index: usize,
        /// The repeated key.
        key: Key,
    },
    /// The author failed validation.
    #[error("commit author: {0}")]
    Author(#[source] IdentityError),
    /// The committer failed validation.
    #[error("commit committer: {0}")]
    Committer(#[source] IdentityError),
    /// The message exceeds [`MAX_MESSAGE_LEN`] bytes.
    #[error("commit message exceeds {} bytes", MAX_MESSAGE_LEN)]
    MessageTooLong,
    /// The message is not valid UTF-8. Unreachable here; see
    /// [`TextError::NotUtf8`].
    #[error("commit message must be valid UTF-8")]
    MessageNotUtf8,
    /// The signature exceeds [`MAX_SIGNATURE_LEN`] bytes.
    #[error("commit signature exceeds {} bytes", MAX_SIGNATURE_LEN)]
    SignatureTooLong,
    /// The public key exceeds [`MAX_PUBLIC_KEY_LEN`] bytes.
    #[error("commit public key exceeds {} bytes", MAX_PUBLIC_KEY_LEN)]
    PublicKeyTooLong,
    /// The input could not be unmarshalled (Go: `"decoding commit: %w"`).
    #[error("decoding commit: {0}")]
    Decode(#[source] CborError),
    /// The input unmarshalled but does not convert to a commit (Go:
    /// `"invalid commit: %w"` around `wireCommit.commit`'s error).
    #[error("invalid commit: {0}")]
    Convert(#[source] ConvertError),
    /// The commit converted but failed validation (Go:
    /// `"invalid commit: %w"` around `validate`'s error).
    #[error("invalid commit: {0}")]
    Invalid(#[source] Box<Error>),
    /// The commit decoded and validated, but its bytes are not the canonical
    /// deterministic encoding (extra or missing map keys, reordered keys,
    /// indefinite-length items, non-minimal heads, tags, ...).
    #[error("commit encoding is not canonical")]
    NotCanonical,
}

/// Who acted and when: git's "Name <email> time tz".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// 1..=[`MAX_IDENTITY_LEN`] bytes, no control characters (CBOR key 0).
    pub name: String,
    /// 0..=[`MAX_IDENTITY_LEN`] bytes, no control characters (CBOR key 1).
    pub email: String,
    /// ns since the Unix epoch (CBOR key 2).
    pub when: i64,
    /// Minutes east of UTC, within ±[`MAX_TZ_OFFSET`] (CBOR key 3). Go's
    /// field is an `int`; the range makes `i32` lossless.
    pub tz_offset: i32,
}

/// A snapshot record. Parents are ordered — the first is the mainline — and
/// empty for a root commit. `signature` and `public_key` are carried opaquely;
/// the core neither creates nor verifies signatures.
///
/// The byte-vector fields carry Go's `omitempty` semantics: an **empty**
/// `signature`/`public_key` means absent (the key is omitted from the
/// encoding), exactly as in [`crate::reference::Reference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// `DirLeaf` or `DirNode` key of the snapshot's root (CBOR key 0).
    pub tree: Key,
    /// `Commit` keys, no duplicates, at most [`MAX_PARENTS`] (CBOR key 1).
    pub parents: Vec<Key>,
    /// Who wrote the change (CBOR key 2).
    pub author: Identity,
    /// Who recorded the commit (CBOR key 3).
    pub committer: Identity,
    /// UTF-8, may be empty, at most [`MAX_MESSAGE_LEN`] bytes (CBOR key 4).
    pub message: String,
    /// Raw SSHSIG blob (CBOR key 5); empty = unsigned.
    pub signature: Vec<u8>,
    /// Signer's key, SSH wire format (CBOR key 6); empty = absent.
    pub public_key: Vec<u8>,
}

/// The unmarshalled, unchecked wire shape of an identity (Go:
/// `wireIdentity`). Filled by the fxamacker-compatible decoder in
/// `fstree::fx`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WireIdentity {
    pub(crate) name: String,
    pub(crate) email: String,
    pub(crate) when: i64,
    pub(crate) tz_offset: i64,
}

/// The unmarshalled, unchecked wire shape of a commit (Go: `wireCommit`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WireCommit {
    pub(crate) tree: Vec<u8>,
    pub(crate) parents: Vec<Vec<u8>>,
    pub(crate) author: WireIdentity,
    pub(crate) committer: WireIdentity,
    pub(crate) message: String,
    pub(crate) signature: Vec<u8>,
    pub(crate) public_key: Vec<u8>,
}

/// Checks an identity string: at most [`MAX_IDENTITY_LEN`] bytes with no
/// control characters (Go: `validateText`; its UTF-8 rule is enforced by
/// `&str` itself here).
fn validate_text(s: &str) -> Result<(), TextError> {
    if s.len() > MAX_IDENTITY_LEN {
        return Err(TextError::TooLong);
    }
    if s.chars().any(|r| r < '\u{20}' || r == '\u{7f}') {
        return Err(TextError::ControlChar);
    }
    Ok(())
}

impl Identity {
    /// Go: `Identity.validate`, same check order.
    fn validate(&self) -> Result<(), IdentityError> {
        if self.name.is_empty() {
            return Err(IdentityError::NameEmpty);
        }
        validate_text(&self.name).map_err(IdentityError::Name)?;
        validate_text(&self.email).map_err(IdentityError::Email)?;
        if !(-MAX_TZ_OFFSET..=MAX_TZ_OFFSET).contains(&self.tz_offset) {
            return Err(IdentityError::TzOffset(i64::from(self.tz_offset)));
        }
        Ok(())
    }

    /// Go: `wireIdentity.identity` — the range check that makes the
    /// narrowing conversion safe.
    fn from_wire(w: WireIdentity) -> Result<Identity, IdentityError> {
        let max = i64::from(MAX_TZ_OFFSET);
        if !(-max..=max).contains(&w.tz_offset) {
            return Err(IdentityError::TzOffset(w.tz_offset));
        }
        Ok(Identity {
            name: w.name,
            email: w.email,
            when: w.when,
            tz_offset: w.tz_offset as i32,
        })
    }

    fn encode_into(&self, b: &mut Vec<u8>) {
        cbor::append_head(b, MAJOR_MAP, 4);
        cbor::append_head(b, MAJOR_UINT, 0);
        cbor::append_tstr(b, &self.name);
        cbor::append_head(b, MAJOR_UINT, 1);
        cbor::append_tstr(b, &self.email);
        cbor::append_head(b, MAJOR_UINT, 2);
        cbor::append_int(b, self.when);
        cbor::append_head(b, MAJOR_UINT, 3);
        cbor::append_int(b, i64::from(self.tz_offset));
    }
}

impl Commit {
    /// Checks the whole record against the rules in
    /// `architecture/commits.md` (Go: `validate`, same check order).
    fn validate(&self) -> Result<(), Error> {
        // `Key::type_` panics on a reserved type nibble; `validate` rules
        // that out first.
        self.tree.validate().map_err(Error::Tree)?;
        let type_ = self.tree.type_();
        if type_ != Type::DirLeaf && type_ != Type::DirNode {
            return Err(Error::TreeNotDirectory {
                key: self.tree,
                type_,
            });
        }
        if self.parents.len() > MAX_PARENTS {
            return Err(Error::TooManyParents(self.parents.len()));
        }
        let mut seen = HashSet::with_capacity(self.parents.len());
        for (index, p) in self.parents.iter().enumerate() {
            p.validate()
                .map_err(|source| Error::Parent { index, source })?;
            if p.type_() != Type::Commit {
                return Err(Error::ParentNotCommit {
                    index,
                    key: *p,
                    type_: p.type_(),
                });
            }
            if !seen.insert(*p) {
                return Err(Error::DuplicateParent { index, key: *p });
            }
        }
        self.author.validate().map_err(Error::Author)?;
        self.committer.validate().map_err(Error::Committer)?;
        if self.message.len() > MAX_MESSAGE_LEN {
            return Err(Error::MessageTooLong);
        }
        if self.signature.len() > MAX_SIGNATURE_LEN {
            return Err(Error::SignatureTooLong);
        }
        if self.public_key.len() > MAX_PUBLIC_KEY_LEN {
            return Err(Error::PublicKeyTooLong);
        }
        Ok(())
    }

    /// Go: `wireCommit.commit`, same conversion order.
    fn from_wire(w: WireCommit) -> Result<Commit, ConvertError> {
        let tree = Key::parse(&w.tree).map_err(ConvertError::Tree)?;
        let mut parents = Vec::with_capacity(w.parents.len());
        for (index, raw) in w.parents.iter().enumerate() {
            parents.push(Key::parse(raw).map_err(|source| ConvertError::Parent { index, source })?);
        }
        let author = Identity::from_wire(w.author).map_err(ConvertError::Author)?;
        let committer = Identity::from_wire(w.committer).map_err(ConvertError::Committer)?;
        Ok(Commit {
            tree,
            parents,
            author,
            committer,
            message: w.message,
            signature: w.signature,
            public_key: w.public_key,
        })
    }

    /// The canonical encoding, without validating first. Infallible — unlike
    /// Go's `encMode.Marshal` there is no error path, so Go's unreachable
    /// `"re-encoding commit: %w"` wrap has no counterpart here.
    fn encode_unchecked(&self) -> Vec<u8> {
        let mut pairs = 5u64;
        if !self.signature.is_empty() {
            pairs += 1;
        }
        if !self.public_key.is_empty() {
            pairs += 1;
        }
        let mut b = Vec::new();
        cbor::append_head(&mut b, MAJOR_MAP, pairs);
        cbor::append_head(&mut b, MAJOR_UINT, 0);
        cbor::append_bstr(&mut b, self.tree.as_bytes());
        cbor::append_head(&mut b, MAJOR_UINT, 1);
        cbor::append_head(&mut b, MAJOR_ARRAY, self.parents.len() as u64);
        for p in &self.parents {
            cbor::append_bstr(&mut b, p.as_bytes());
        }
        cbor::append_head(&mut b, MAJOR_UINT, 2);
        self.author.encode_into(&mut b);
        cbor::append_head(&mut b, MAJOR_UINT, 3);
        self.committer.encode_into(&mut b);
        cbor::append_head(&mut b, MAJOR_UINT, 4);
        cbor::append_tstr(&mut b, &self.message);
        if !self.signature.is_empty() {
            cbor::append_head(&mut b, MAJOR_UINT, 5);
            cbor::append_bstr(&mut b, &self.signature);
        }
        if !self.public_key.is_empty() {
            cbor::append_head(&mut b, MAJOR_UINT, 6);
            cbor::append_bstr(&mut b, &self.public_key);
        }
        b
    }

    /// Returns the deterministic CBOR encoding of a validated commit (Go:
    /// `Encode`).
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        Ok(self.encode_unchecked())
    }

    /// Encodes the commit and derives its key: type `Commit`, length field =
    /// the encoding's own byte length (Go: `Object`).
    pub fn object(&self) -> Result<(Key, Vec<u8>), Error> {
        let b = self.encode()?;
        let k = Key::new(Type::Commit, b.len() as u64, &b);
        Ok((k, b))
    }

    /// Returns the bytes a signature runs over: the deterministic encoding
    /// without the `signature` field (Go: `SignaturePayload`). `public_key`
    /// stays in, so the payload binds the signer's key; set it before
    /// computing the payload.
    pub fn signature_payload(&self) -> Result<Vec<u8>, Error> {
        let mut c = self.clone();
        c.signature = Vec::new();
        c.encode()
    }

    /// Parses and validates a commit (Go: `Decode`). It rejects non-canonical
    /// encodings: the input must be byte-for-byte what [`Commit::encode`]
    /// produces for the same record. The stages and their order are Go's:
    /// lax fxamacker-compatible unmarshal, conversion, validation, then the
    /// canonical re-encoding comparison — so both implementations accept
    /// exactly the canonical encodings of valid commits and classify every
    /// rejection alike.
    pub fn decode(b: &[u8]) -> Result<Commit, Error> {
        let w = fstree::unmarshal_commit(b).map_err(Error::Decode)?;
        let c = Commit::from_wire(w).map_err(Error::Convert)?;
        if let Err(e) = c.validate() {
            return Err(Error::Invalid(Box::new(e)));
        }
        if c.encode_unchecked() != b {
            return Err(Error::NotCanonical);
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    //! Ports of Go `commit/commit_test.go`.

    use super::*;
    use crate::key::Type;

    const ANN_WHEN: i64 = 1_700_000_000_000_000_000;
    const BOB_WHEN: i64 = ANN_WHEN + 1;

    fn ann() -> Identity {
        Identity {
            name: "Ann".into(),
            email: "ann@example.com".into(),
            when: ANN_WHEN,
            tz_offset: 120,
        }
    }

    fn bob() -> Identity {
        Identity {
            name: "Bob".into(),
            email: String::new(),
            when: BOB_WHEN,
            tz_offset: -300,
        }
    }

    /// The key of the empty directory: a DirLeaf whose body is the empty CBOR
    /// array 0x80, length field 1.
    fn empty_dir() -> Key {
        Key::new(Type::DirLeaf, 1, &[0x80])
    }

    /// A parentless commit of the empty directory and its key.
    fn root(msg: &str) -> (Key, Commit) {
        let c = Commit {
            tree: empty_dir(),
            parents: Vec::new(),
            author: ann(),
            committer: ann(),
            message: msg.into(),
            signature: Vec::new(),
            public_key: Vec::new(),
        };
        let (k, _) = c.object().expect("root commit encodes");
        (k, c)
    }

    /// The fixed vector commit: parents root("a") and root("b").
    fn merge() -> Commit {
        Commit {
            tree: empty_dir(),
            parents: vec![root("a").0, root("b").0],
            author: ann(),
            committer: bob(),
            message: "merge\n".into(),
            signature: Vec::new(),
            public_key: Vec::new(),
        }
    }

    // Hand-rolled CBOR, independent of the encoder under test.
    fn bstr32(k: Key) -> Vec<u8> {
        let mut v = vec![0x58, 0x20];
        v.extend_from_slice(k.as_bytes());
        v
    }

    fn u64be(n: i64) -> Vec<u8> {
        let mut v = vec![0x1b];
        v.extend_from_slice(&(n as u64).to_be_bytes());
        v
    }

    fn tstr(s: &str) -> Vec<u8> {
        assert!(s.len() < 24, "tstr: short strings only");
        let mut v = vec![0x60 | s.len() as u8];
        v.extend_from_slice(s.as_bytes());
        v
    }

    /// merge()'s canonical bytes, assembled from the tables in
    /// `architecture/commits.md`.
    fn hand_merge() -> Vec<u8> {
        let (ka, kb) = (root("a").0, root("b").0);
        [
            vec![0xa5], // map(5)
            vec![0x00],
            bstr32(empty_dir()),
            vec![0x01, 0x82], // array(2)
            bstr32(ka),
            bstr32(kb),
            vec![0x02, 0xa4, 0x00],
            tstr("Ann"),
            vec![0x01],
            tstr("ann@example.com"),
            vec![0x02],
            u64be(ANN_WHEN),
            vec![0x03, 0x18, 0x78], // +120
            vec![0x03, 0xa4, 0x00],
            tstr("Bob"),
            vec![0x01],
            tstr(""),
            vec![0x02],
            u64be(BOB_WHEN),
            vec![0x03, 0x39, 0x01, 0x2b], // -300
            vec![0x04],
            tstr("merge\n"),
        ]
        .concat()
    }

    /// Swaps the single occurrence of `old` in `b`.
    fn replace_once(b: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
        let hits: Vec<usize> = (0..=b.len() - old.len())
            .filter(|&i| &b[i..i + old.len()] == old)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "pattern {old:02x?} occurs {} times",
            hits.len()
        );
        let i = hits[0];
        [&b[..i], new, &b[i + old.len()..]].concat()
    }

    #[test]
    fn encode_matches_hand_assembled_bytes() {
        let got = merge().encode().unwrap();
        assert_eq!(hex::encode(got), hex::encode(hand_merge()));
    }

    #[test]
    fn encode_decode_round_trip() {
        let mut signed = merge();
        signed.signature = b"sig".to_vec();
        signed.public_key = b"pub".to_vec();
        for (name, c) in [
            ("merge", merge()),
            ("signed", signed),
            ("root, empty message", root("").1),
        ] {
            let b = c.encode().unwrap_or_else(|e| panic!("{name}: encode: {e}"));
            let got = Commit::decode(&b).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
            assert_eq!(got, c, "{name}: round trip");
        }
    }

    #[test]
    fn object_key() {
        let (k, b) = merge().object().unwrap();
        assert_eq!(k.type_(), Type::Commit);
        assert_eq!(
            k.length(),
            b.len() as u64,
            "length field is the own byte length"
        );
        assert_eq!(k, Key::new(Type::Commit, b.len() as u64, &b));
    }

    #[test]
    fn signature_payload() {
        let mut c = merge();
        c.public_key = b"pub".to_vec();
        let unsigned = c.encode().unwrap();
        c.signature = b"sig".to_vec();
        let payload = c.signature_payload().unwrap();
        assert_eq!(
            payload, unsigned,
            "payload is the encoding without the signature"
        );
        assert_ne!(
            c.encode().unwrap(),
            payload,
            "payload must not carry the signature"
        );
        c.public_key = b"other".to_vec();
        assert_ne!(
            c.signature_payload().unwrap(),
            payload,
            "payload covers the public key"
        );
    }

    /// `n` distinct canonical Commit-type keys.
    fn commit_keys(n: usize) -> Vec<Key> {
        (0..n)
            .map(|i| {
                let mut h = [0u8; key::SIZE];
                h[..4].copy_from_slice(&(i as u32 + 1).to_be_bytes());
                Key::new_from_hash(Type::Commit, 1, h)
            })
            .collect()
    }

    type Mutation = (&'static str, fn(&mut Commit));

    #[test]
    fn encode_accepts_bounds() {
        let cases: [Mutation; 10] = [
            ("max parents", |c| c.parents = commit_keys(MAX_PARENTS)),
            ("tz upper", |c| c.author.tz_offset = MAX_TZ_OFFSET),
            ("tz lower", |c| c.author.tz_offset = -MAX_TZ_OFFSET),
            ("max message", |c| c.message = "m".repeat(MAX_MESSAGE_LEN)),
            ("multiline", |c| c.message = "subject\n\n\tbody\n".into()),
            ("max name", |c| c.author.name = "n".repeat(MAX_IDENTITY_LEN)),
            ("negative time", |c| c.author.when = -1),
            ("max signature", |c| {
                c.signature = vec![0; MAX_SIGNATURE_LEN]
            }),
            ("max public key", |c| {
                c.public_key = vec![0; MAX_PUBLIC_KEY_LEN]
            }),
            ("dir node as tree", |c| {
                let mut h = [0u8; key::SIZE];
                h[0] = 1;
                c.tree = Key::new_from_hash(Type::DirNode, 9, h);
            }),
        ];
        for (name, mutate) in cases {
            let mut c = merge();
            mutate(&mut c);
            let b = c.encode().unwrap_or_else(|e| panic!("{name}: encode: {e}"));
            let got = Commit::decode(&b).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
            assert_eq!(got, c, "{name}: round trip");
        }
    }

    /// Go's two invalid-UTF-8 cases (name, message) are unrepresentable here:
    /// `String` is UTF-8 by construction.
    #[test]
    fn encode_rejects_invalid() {
        let tree = empty_dir().to_string();
        let parent_a = root("a").0.to_string();
        let cases: [(Mutation, String); 17] = [
            (
                ("zero tree", |c| c.tree = Key([0u8; key::SIZE])),
                format!(
                    "commit tree {} is not a directory key (type Blob)",
                    "00".repeat(32)
                ),
            ),
            (
                ("file tree", |c| {
                    let mut h = [0u8; key::SIZE];
                    h[0] = 1;
                    c.tree = Key::new_from_hash(Type::FileNode, 9, h);
                }),
                format!(
                    "commit tree 100901{} is not a directory key (type FileNode)",
                    "00".repeat(29)
                ),
            ),
            (
                ("non-canonical tree", |c| c.tree.0[0] |= 0x08),
                "commit tree: key: reserved header bit is set".into(),
            ),
            (
                ("parent not a commit", |c| c.parents[0] = c.tree),
                format!("commit parent 0: {tree} is not a commit key (type DirLeaf)"),
            ),
            (
                ("non-canonical parent", |c| c.parents[1].0[0] = 0xf0),
                "commit parent 1: key: reserved object type: 15".into(),
            ),
            (
                ("duplicate parents", |c| c.parents[1] = c.parents[0]),
                format!("commit parent 1: duplicate {parent_a}"),
            ),
            (
                ("too many parents", |c| {
                    c.parents = commit_keys(MAX_PARENTS + 1)
                }),
                "commit has 257 parents, more than 256".into(),
            ),
            (
                ("empty author name", |c| c.author.name = String::new()),
                "commit author: name must not be empty".into(),
            ),
            (
                ("empty committer name", |c| c.committer.name = String::new()),
                "commit committer: name must not be empty".into(),
            ),
            (
                ("control char in name", |c| c.author.name = "An\nn".into()),
                "commit author: name must not contain control characters".into(),
            ),
            (
                ("DEL in email", |c| c.author.email = "a\x7fb".into()),
                "commit author: email must not contain control characters".into(),
            ),
            (
                ("name too long", |c| {
                    c.author.name = "n".repeat(MAX_IDENTITY_LEN + 1)
                }),
                "commit author: name exceeds 1024 bytes".into(),
            ),
            (
                ("email too long", |c| {
                    c.committer.email = "e".repeat(MAX_IDENTITY_LEN + 1)
                }),
                "commit committer: email exceeds 1024 bytes".into(),
            ),
            (
                ("tz too far east", |c| {
                    c.author.tz_offset = MAX_TZ_OFFSET + 1
                }),
                "commit author: tz offset 1440 outside ±1439 minutes".into(),
            ),
            (
                ("tz too far west", |c| {
                    c.committer.tz_offset = -MAX_TZ_OFFSET - 1
                }),
                "commit committer: tz offset -1440 outside ±1439 minutes".into(),
            ),
            (
                ("message too long", |c| {
                    c.message = "m".repeat(MAX_MESSAGE_LEN + 1)
                }),
                "commit message exceeds 1048576 bytes".into(),
            ),
            (
                ("signature too long", |c| {
                    c.signature = vec![0; MAX_SIGNATURE_LEN + 1]
                }),
                "commit signature exceeds 65536 bytes".into(),
            ),
        ];
        for ((name, mutate), want) in cases {
            let mut c = merge();
            mutate(&mut c);
            let err = c.encode().expect_err(name);
            assert_eq!(err.to_string(), want, "{name}");
            assert_eq!(c.object().expect_err(name), err, "{name}: object");
        }
        let mut c = merge();
        c.public_key = vec![0; MAX_PUBLIC_KEY_LEN + 1];
        assert_eq!(
            c.encode().unwrap_err().to_string(),
            "commit public key exceeds 16384 bytes"
        );
    }

    #[test]
    fn decode_rejects() {
        let good = hand_merge();
        Commit::decode(&good).expect("baseline must decode");
        let (ka, kb) = (root("a").0, root("b").0);
        let tree = empty_dir();
        let message = [vec![0x04], tstr("merge\n")].concat();
        let parents = [vec![0x01, 0x82], bstr32(ka), bstr32(kb)].concat();
        let with_header = |b: &[u8], h: u8| {
            let mut out = b.to_vec();
            out[0] = h;
            out
        };
        const NOT_CANONICAL: &str = "commit encoding is not canonical";
        let cases: Vec<(&str, Vec<u8>, String)> = vec![
            (
                "garbage",
                b"not cbor at all".to_vec(),
                "decoding commit: cbor: cannot unmarshal UTF-8 text string into Go value of type commit.wireCommit"
                    .into(),
            ),
            ("empty", Vec::new(), "decoding commit: EOF".into()),
            (
                "trailing byte",
                [good.clone(), vec![0x00]].concat(),
                "decoding commit: cbor: 1 bytes of extraneous data starting at index 174".into(),
            ),
            // Accepted by the CBOR library's defaults; only the canonical
            // re-encoding check stands between these and a second encoding of
            // the same commit.
            (
                "self-described tag",
                [vec![0xd9, 0xd9, 0xf7], good.clone()].concat(),
                NOT_CANONICAL.into(),
            ),
            (
                "duplicate key 4",
                [with_header(&good, 0xa6), message.clone()].concat(),
                NOT_CANONICAL.into(),
            ),
            (
                "bignum-tagged tz",
                replace_once(&good, &[0x03, 0x18, 0x78], &[0x03, 0xc2, 0x41, 0x78]),
                NOT_CANONICAL.into(),
            ),
            (
                "unknown key 7",
                [with_header(&good, 0xa6), vec![0x07, 0x00]].concat(),
                NOT_CANONICAL.into(),
            ),
            (
                "missing message",
                with_header(&good[..good.len() - message.len()], 0xa4),
                NOT_CANONICAL.into(),
            ),
            (
                "non-minimal tz",
                replace_once(&good, &[0x03, 0x18, 0x78], &[0x03, 0x19, 0x00, 0x78]),
                NOT_CANONICAL.into(),
            ),
            (
                "tz out of range",
                replace_once(&good, &[0x03, 0x18, 0x78], &[0x03, 0x19, 0x05, 0xa0]), // 1440
                "invalid commit: author: tz offset 1440 outside ±1439 minutes".into(),
            ),
            (
                "identity missing email",
                replace_once(
                    &good,
                    &[vec![0xa4, 0x00], tstr("Bob"), vec![0x01], tstr("")].concat(),
                    &[vec![0xa3, 0x00], tstr("Bob")].concat(),
                ),
                NOT_CANONICAL.into(),
            ),
            (
                "keys out of order",
                replace_once(
                    &good,
                    &[vec![0x00], bstr32(tree), parents.clone()].concat(),
                    &[parents.clone(), vec![0x00], bstr32(tree)].concat(),
                ),
                NOT_CANONICAL.into(),
            ),
            (
                "parents null",
                replace_once(&good, &parents, &[0x01, 0xf6]),
                NOT_CANONICAL.into(),
            ),
            (
                "indefinite parents",
                replace_once(
                    &good,
                    &parents,
                    &[vec![0x01, 0x9f], bstr32(ka), bstr32(kb), vec![0xff]].concat(),
                ),
                NOT_CANONICAL.into(),
            ),
            (
                "parent is a tree",
                replace_once(
                    &good,
                    &[bstr32(ka), bstr32(kb)].concat(),
                    &[bstr32(tree), bstr32(kb)].concat(),
                ),
                format!("invalid commit: commit parent 0: {tree} is not a commit key (type DirLeaf)"),
            ),
            (
                "duplicate parent",
                replace_once(
                    &good,
                    &[bstr32(ka), bstr32(kb)].concat(),
                    &[bstr32(ka), bstr32(ka)].concat(),
                ),
                format!("invalid commit: commit parent 1: duplicate {ka}"),
            ),
            (
                "short tree key",
                replace_once(
                    &good,
                    &[vec![0x00], bstr32(tree)].concat(),
                    &[vec![0x00, 0x58, 0x1f], tree.as_bytes()[..31].to_vec()].concat(),
                ),
                "invalid commit: tree: key: data is not 32 bytes: got 31".into(),
            ),
            (
                "message as bytes",
                replace_once(&good, &message, &[vec![0x04, 0x46], b"merge\n".to_vec()].concat()),
                "decoding commit: cbor: cannot unmarshal byte string into Go struct field commit.wireCommit.4 of type string"
                    .into(),
            ),
            (
                "author name is a number",
                replace_once(
                    &good,
                    &[vec![0x02, 0xa4, 0x00], tstr("Ann")].concat(),
                    &[0x02, 0xa4, 0x00, 0x07],
                ),
                // The outer struct's field name overwrites the inner one; the
                // Go type stays the inner field's.
                "decoding commit: cbor: cannot unmarshal positive integer into Go struct field commit.wireCommit.2 of type string"
                    .into(),
            ),
        ];
        for (name, b, want) in cases {
            let err = Commit::decode(&b).expect_err(name);
            assert_eq!(err.to_string(), want, "{name}");
        }
    }

    // Golden vector: the fixed merge commit, pinned as literal bytes. The same
    // constants are pinned in Go's `commit/commit_test.go` and annotated in
    // `architecture/commits.md`.
    const GOLDEN_PARENT_A: &str =
        "5073d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c";
    const GOLDEN_PARENT_B: &str =
        "5073c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe48831";
    const GOLDEN_KEY: &str = "50ae7b19332c07e0f197c8a9d410c0cc3ed2d7fd6cf34d3a7cb3c43d6c514980";
    const GOLDEN_BYTES: &str = "a50058202001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b018258205073d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c58205073c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe4883102a40063416e6e016f616e6e406578616d706c652e636f6d021b17979cfe362a000003187803a40063426f620160021b17979cfe362a00010339012b04666d657267650a";

    #[test]
    fn golden_vector() {
        assert_eq!(root("a").0.to_string(), GOLDEN_PARENT_A);
        assert_eq!(root("b").0.to_string(), GOLDEN_PARENT_B);
        let (k, b) = merge().object().unwrap();
        assert_eq!(hex::encode(&b), GOLDEN_BYTES);
        assert_eq!(k.to_string(), GOLDEN_KEY);
        let decoded = Commit::decode(&hex::decode(GOLDEN_BYTES).unwrap()).unwrap();
        assert_eq!(decoded, merge());
    }
}
