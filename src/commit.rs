//! The Commit object (CAS type 5): the analogue of a git commit — a directory
//! root, ordered parent commits, author, committer and message, with an
//! optional opaque signature — extended with what a jj commit carries
//! besides: a change id, and the further terms and the labels of a conflicted
//! tree (Go package `commit`).
//!
//! Encoding is RFC 8949 §4.2 core-deterministic CBOR (canonical map, integer
//! keys), the fstree and reference convention. The key's length field is a
//! footprint, like a directory's: the commit's own bytes plus its trees. See
//! `architecture/commits.md`.

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

/// The maximum [`Commit::change_id`] length in bytes (Go: `MaxChangeIDLen`).
pub const MAX_CHANGE_ID_LEN: usize = 64;

/// The maximum number of conflict terms after the tree (Go:
/// `MaxConflictTerms`). The number is always even: a remove for every further
/// add.
pub const MAX_CONFLICT_TERMS: usize = 254;

/// The maximum byte length of one conflict label, 64 KiB (Go: `MaxLabelLen`).
/// jj makes a label from a commit's short ids and the whole first line of its
/// description, which nothing bounds; its adapter truncates beyond this.
pub const MAX_LABEL_LEN: usize = 64 << 10;

/// A violation of the rules for an identity string or a conflict label (Go:
/// `validateText`). `Display` reproduces the Go diagnostic, which the caller
/// prefixes with the field name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The string exceeds its bound — [`MAX_IDENTITY_LEN`] for an identity
    /// string, [`MAX_LABEL_LEN`] for a conflict label; carries the bound.
    #[error("exceeds {0} bytes")]
    TooLong(usize),
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
    /// An element of key 8 is not a canonical 32-byte key.
    #[error("conflict term {index}: {source}")]
    ConflictTerm {
        /// The term's position in [`Commit::conflict_terms`].
        index: usize,
        /// The key parse failure.
        source: key::Error,
    },
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
    /// The change id exceeds [`MAX_CHANGE_ID_LEN`] bytes.
    #[error("commit change id exceeds {} bytes", MAX_CHANGE_ID_LEN)]
    ChangeIdTooLong,
    /// An odd number of conflict terms, or more than [`MAX_CONFLICT_TERMS`];
    /// carries the count.
    #[error(
        "commit has {0} conflict terms, want an even number up to {max}: a remove for every further add",
        max = MAX_CONFLICT_TERMS
    )]
    ConflictTermCount(usize),
    /// A conflict term is not a canonical key.
    #[error("commit conflict term {index}: {source}")]
    ConflictTerm {
        /// The term's position in [`Commit::conflict_terms`].
        index: usize,
        /// The key validation failure.
        source: key::Error,
    },
    /// A conflict term is not a `DirLeaf` or `DirNode` key.
    #[error("commit conflict term {index}: {key} is not a directory key (type {type_})")]
    ConflictTermNotDirectory {
        /// The term's position in [`Commit::conflict_terms`].
        index: usize,
        /// The offending term.
        key: Key,
        /// Its object type.
        type_: Type,
    },
    /// Conflict labels on a commit whose tree is resolved.
    #[error("commit has conflict labels but no conflict")]
    LabelsWithoutConflict,
    /// The number of labels is not one per term counting the tree.
    #[error("commit has {labels} conflict labels for {want} terms")]
    LabelCount {
        /// How many labels the commit carries.
        labels: usize,
        /// How many it has to carry: `1 + conflict_terms.len()`.
        want: usize,
    },
    /// A conflict label broke a string rule.
    #[error("commit conflict label {index} {source}")]
    Label {
        /// The label's position; 0 labels the tree.
        index: usize,
        /// The rule it broke.
        source: TextError,
    },
    /// Every conflict label is empty.
    #[error("commit conflict labels are all empty; an unlabelled conflict carries none")]
    LabelsAllEmpty,
    /// The commit's own bytes plus the lengths of its trees do not fit 64
    /// bits ([`footprint`]); such a commit has no key.
    #[error("commit footprint overflows the key's length field")]
    FootprintOverflow,
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
    /// 0..=[`MAX_IDENTITY_LEN`] bytes, no control characters; empty when the
    /// user configured none (CBOR key 0).
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
/// A conflicted commit records the tree A0 − R0 + A1 − R1 + …: `tree` holds
/// A0 and `conflict_terms` holds R0, A1, R1, A2, … (jj's order), so their
/// number is even. Wherever a commit stands for a directory it stands for
/// `tree`.
///
/// The optional fields carry Go's `omitempty` semantics: an **empty**
/// `signature`, `public_key`, `change_id`, `conflict_terms` or
/// `conflict_labels` means absent (the key is omitted from the encoding),
/// exactly as in [`crate::reference::Reference`].
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
    /// Opaque, 1..=[`MAX_CHANGE_ID_LEN`] bytes: follows the change through
    /// rewrites (CBOR key 7); empty = absent.
    pub change_id: Vec<u8>,
    /// `DirLeaf` or `DirNode` keys, an even number up to
    /// [`MAX_CONFLICT_TERMS`], repeats allowed (CBOR key 8); empty when the
    /// tree is resolved.
    pub conflict_terms: Vec<Key>,
    /// One per term counting `tree`, each at most [`MAX_LABEL_LEN`] bytes
    /// with no control characters, at least one non-empty (CBOR key 9); empty
    /// when no term is labelled.
    pub conflict_labels: Vec<String>,
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
    pub(crate) change_id: Vec<u8>,
    pub(crate) conflict_terms: Vec<Vec<u8>>,
    pub(crate) conflict_labels: Vec<String>,
}

/// Checks an identity string or a conflict label: at most `max` bytes with
/// no control characters (Go: `validateText`; its UTF-8 rule is enforced by
/// `&str` itself here).
fn validate_text(s: &str, max: usize) -> Result<(), TextError> {
    if s.len() > max {
        return Err(TextError::TooLong(max));
    }
    if s.chars().any(|r| r < '\u{20}' || r == '\u{7f}') {
        return Err(TextError::ControlChar);
    }
    Ok(())
}

impl Identity {
    /// Go: `Identity.validate`, same check order.
    fn validate(&self) -> Result<(), IdentityError> {
        // An empty name is what a user who configured none commits under (jj).
        validate_text(&self.name, MAX_IDENTITY_LEN).map_err(IdentityError::Name)?;
        validate_text(&self.email, MAX_IDENTITY_LEN).map_err(IdentityError::Email)?;
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
        if self.change_id.len() > MAX_CHANGE_ID_LEN {
            return Err(Error::ChangeIdTooLong);
        }
        let n = self.conflict_terms.len();
        if !n.is_multiple_of(2) || n > MAX_CONFLICT_TERMS {
            return Err(Error::ConflictTermCount(n));
        }
        for (index, t) in self.conflict_terms.iter().enumerate() {
            t.validate()
                .map_err(|source| Error::ConflictTerm { index, source })?;
            let type_ = t.type_();
            if type_ != Type::DirLeaf && type_ != Type::DirNode {
                return Err(Error::ConflictTermNotDirectory {
                    index,
                    key: *t,
                    type_,
                });
            }
        }
        if !self.conflict_labels.is_empty() {
            if !self.conflicted() {
                return Err(Error::LabelsWithoutConflict);
            }
            let want = 1 + self.conflict_terms.len();
            if self.conflict_labels.len() != want {
                return Err(Error::LabelCount {
                    labels: self.conflict_labels.len(),
                    want,
                });
            }
            let mut labelled = false;
            for (index, l) in self.conflict_labels.iter().enumerate() {
                validate_text(l, MAX_LABEL_LEN).map_err(|source| Error::Label { index, source })?;
                labelled = labelled || !l.is_empty();
            }
            if !labelled {
                return Err(Error::LabelsAllEmpty);
            }
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
        // An array or byte string that is present but empty converts to the
        // same empty value as an absent one, re-encodes to nothing, and so
        // fails decode's canonical check: absence has one encoding.
        let mut conflict_terms = Vec::with_capacity(w.conflict_terms.len());
        for (index, raw) in w.conflict_terms.iter().enumerate() {
            conflict_terms.push(
                Key::parse(raw).map_err(|source| ConvertError::ConflictTerm { index, source })?,
            );
        }
        Ok(Commit {
            tree,
            parents,
            author,
            committer,
            message: w.message,
            signature: w.signature,
            public_key: w.public_key,
            change_id: w.change_id,
            conflict_terms,
            conflict_labels: w.conflict_labels,
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
        if !self.change_id.is_empty() {
            pairs += 1;
        }
        if !self.conflict_terms.is_empty() {
            pairs += 1;
        }
        if !self.conflict_labels.is_empty() {
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
        if !self.change_id.is_empty() {
            cbor::append_head(&mut b, MAJOR_UINT, 7);
            cbor::append_bstr(&mut b, &self.change_id);
        }
        if !self.conflict_terms.is_empty() {
            cbor::append_head(&mut b, MAJOR_UINT, 8);
            cbor::append_head(&mut b, MAJOR_ARRAY, self.conflict_terms.len() as u64);
            for t in &self.conflict_terms {
                cbor::append_bstr(&mut b, t.as_bytes());
            }
        }
        if !self.conflict_labels.is_empty() {
            cbor::append_head(&mut b, MAJOR_UINT, 9);
            cbor::append_head(&mut b, MAJOR_ARRAY, self.conflict_labels.len() as u64);
            for l in &self.conflict_labels {
                cbor::append_tstr(&mut b, l);
            }
        }
        b
    }

    /// Returns the deterministic CBOR encoding of a validated commit (Go:
    /// `Encode`).
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        Ok(self.encode_unchecked())
    }

    /// Encodes the commit and derives its key: type `Commit`, the length
    /// field a footprint as a directory's is — the encoding's own byte length
    /// plus the length of every tree ([`Commit::trees`]) (Go: `Object`).
    /// Parents are not counted: a parent's length would hold its own
    /// parents', so every merge would count the history its parents share
    /// twice, roughly doubling the value until it no longer fit; and a
    /// directory that holds a commit reports, through this length, the size
    /// of what is beneath it, not of its history.
    pub fn object(&self) -> Result<(Key, Vec<u8>), Error> {
        let b = self.encode()?;
        let length = footprint(b.len() as u64, &self.trees())?;
        let k = Key::new(Type::Commit, length, &b);
        Ok((k, b))
    }

    /// Returns every tree the commit records: `tree`, then the conflict terms
    /// in order (Go: `Trees`). The key's length and the object graph both go
    /// by this list.
    pub fn trees(&self) -> Vec<Key> {
        let mut out = Vec::with_capacity(1 + self.conflict_terms.len());
        out.push(self.tree);
        out.extend_from_slice(&self.conflict_terms);
        out
    }

    /// Reports whether the commit records a conflicted tree (Go:
    /// `Conflicted`).
    pub fn conflicted(&self) -> bool {
        !self.conflict_terms.is_empty()
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

/// The length field of the key of a commit whose encoding is `own` bytes long
/// and which records `trees` (Go: `Footprint`). It is an error
/// ([`Error::FootprintOverflow`]) for the sum not to fit.
pub fn footprint(own: u64, trees: &[Key]) -> Result<u64, Error> {
    let mut length = own;
    for t in trees {
        length = length
            .checked_add(t.length())
            .ok_or(Error::FootprintOverflow)?;
    }
    Ok(length)
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
            change_id: Vec::new(),
            conflict_terms: Vec::new(),
            conflict_labels: Vec::new(),
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
            change_id: Vec::new(),
            conflict_terms: Vec::new(),
            conflict_labels: Vec::new(),
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
        let footprint = b.len() as u64 + empty_dir().length();
        assert_eq!(
            k.length(),
            footprint,
            "length field is own bytes plus the tree's length"
        );
        assert_eq!(k, Key::new(Type::Commit, footprint, &b));
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
        let cases: [Mutation; 11] = [
            ("empty names", |c| {
                c.author.name = String::new();
                c.committer.name = String::new();
            }),
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
        let cases: [(Mutation, String); 15] = [
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
                "key 7 not bytes",
                [with_header(&good, 0xa6), vec![0x07, 0x00]].concat(),
                "decoding commit: cbor: cannot unmarshal positive integer into Go struct field commit.wireCommit.7 of type []uint8"
                    .into(),
            ),
            (
                "unknown key 10",
                [with_header(&good, 0xa6), vec![0x0a, 0x00]].concat(),
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
    // `architecture/commits.md`. A key's length field is the commit's own
    // bytes plus its tree's length: 115+1 for the parents, 174+1 for the merge.
    const GOLDEN_PARENT_A: &str =
        "5074d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c";
    const GOLDEN_PARENT_B: &str =
        "5074c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe48831";
    const GOLDEN_KEY: &str = "50afd48693e2ae8418fb83d0459174df45ba5fdddef67376e74ac6893a5abbed";
    const GOLDEN_BYTES: &str = "a50058202001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b018258205074d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c58205074c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe4883102a40063416e6e016f616e6e406578616d706c652e636f6d021b17979cfe362a000003187803a40063426f620160021b17979cfe362a00010339012b04666d657267650a";

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

    // --- what a jj commit carries besides (Go PR #15) ---

    /// Fabricates a directory key: the codec never fetches a tree, so any
    /// canonical DirLeaf or DirNode key will do (Go `dirKey`).
    fn dir_key(t: Type, length: u64, fill: u8) -> Key {
        Key::new_from_hash(t, length, [fill; key::SIZE])
    }

    /// `n` distinct DirLeaf keys.
    fn dir_keys(n: usize) -> Vec<Key> {
        (0..n)
            .map(|i| dir_key(Type::DirLeaf, i as u64 + 1, i as u8))
            .collect()
    }

    /// A Blob key, which no tree or term may be.
    fn blob_key() -> Key {
        let mut h = [0u8; key::SIZE];
        h[0] = 1;
        Key::new_from_hash(Type::Blob, 9, h)
    }

    const CHANGE_ID: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    /// The second fixed vector: a three-term conflict with labels, a change
    /// id, and a committer without a name (Go `conflicted`).
    fn conflicted() -> Commit {
        Commit {
            tree: empty_dir(),
            parents: vec![root("a").0],
            author: ann(),
            committer: Identity {
                name: String::new(),
                email: "bot@example.com".into(),
                when: BOB_WHEN,
                tz_offset: -300,
            },
            message: "conflict\n".into(),
            signature: Vec::new(),
            public_key: Vec::new(),
            change_id: CHANGE_ID.to_vec(),
            conflict_terms: vec![
                dir_key(Type::DirLeaf, 300, 0x11),
                dir_key(Type::DirNode, 70000, 0x22),
            ],
            conflict_labels: vec!["ours".into(), String::new(), "theirs".into()],
        }
    }

    /// conflicted()'s canonical bytes, assembled from the spec's tables.
    fn hand_conflicted() -> Vec<u8> {
        let c = conflicted();
        [
            vec![0xa8], // map(8): keys 0-4, 7, 8, 9
            vec![0x00],
            bstr32(empty_dir()),
            vec![0x01, 0x81],
            bstr32(root("a").0),
            vec![0x02, 0xa4, 0x00],
            tstr("Ann"),
            vec![0x01],
            tstr("ann@example.com"),
            vec![0x02],
            u64be(ANN_WHEN),
            vec![0x03, 0x18, 0x78],
            vec![0x03, 0xa4, 0x00],
            tstr(""),
            vec![0x01],
            tstr("bot@example.com"),
            vec![0x02],
            u64be(BOB_WHEN),
            vec![0x03, 0x39, 0x01, 0x2b],
            vec![0x04],
            tstr("conflict\n"),
            vec![0x07, 0x50], // bytes(16)
            CHANGE_ID.to_vec(),
            vec![0x08, 0x82],
            bstr32(c.conflict_terms[0]),
            bstr32(c.conflict_terms[1]),
            vec![0x09, 0x83],
            tstr("ours"),
            tstr(""),
            tstr("theirs"),
        ]
        .concat()
    }

    #[test]
    fn conflicted_commit_matches_hand_assembled_bytes() {
        let got = conflicted().encode().unwrap();
        assert_eq!(hex::encode(got), hex::encode(hand_conflicted()));
    }

    #[test]
    fn new_fields_round_trip() {
        let mut with_id = merge();
        with_id.change_id = vec![0xab];
        let mut terms = merge();
        terms.conflict_terms = conflicted().conflict_terms;
        let mut signed = conflicted();
        signed.signature = b"sig".to_vec();
        signed.public_key = b"pub".to_vec();
        for (name, c) in [
            ("change id", with_id),
            ("terms", terms),
            ("terms and labels", conflicted()),
            ("everything", signed),
        ] {
            let b = c.encode().unwrap_or_else(|e| panic!("{name}: encode: {e}"));
            let got = Commit::decode(&b).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
            assert_eq!(got, c, "{name}: round trip");
        }
        let c = merge();
        assert!(!c.conflicted(), "a resolved commit");
        assert_eq!(c.trees(), vec![c.tree]);
        let c = conflicted();
        assert!(c.conflicted(), "a conflicted commit");
        assert_eq!(
            c.trees(),
            vec![c.tree, c.conflict_terms[0], c.conflict_terms[1]]
        );
    }

    #[test]
    fn encode_accepts_jj_bounds() {
        let cases: [Mutation; 9] = [
            ("max terms", |c| {
                c.conflict_terms = dir_keys(MAX_CONFLICT_TERMS);
                c.conflict_labels = Vec::new();
            }),
            ("a term repeats", |c| {
                c.conflict_terms[1] = c.conflict_terms[0]
            }),
            ("terms, no labels", |c| c.conflict_labels = Vec::new()),
            ("max label", |c| {
                c.conflict_labels[1] = "l".repeat(MAX_LABEL_LEN)
            }),
            // jj labels carry a description's whole first line
            ("a long subject", |c| {
                c.conflict_labels[1] = format!(
                    "wqnwkozp 2768b0b9 \"{}\"",
                    "a paragraph on one line ".repeat(200)
                )
            }),
            // Only code points below U+0020 and U+007F are refused: C1
            // controls and the Unicode line separators pass, as in Go.
            ("C1 and separators pass", |c| {
                c.conflict_labels[0] = "\u{85}\u{2028}\u{9f}".into()
            }),
            ("one-byte id", |c| c.change_id = vec![0]),
            ("max id", |c| c.change_id = vec![0; MAX_CHANGE_ID_LEN]),
            ("nobody at all", |c| {
                c.author = Identity::default();
                c.committer = Identity::default();
            }),
        ];
        for (name, mutate) in cases {
            let mut c = conflicted();
            mutate(&mut c);
            let b = c.encode().unwrap_or_else(|e| panic!("{name}: encode: {e}"));
            let got = Commit::decode(&b).unwrap_or_else(|e| panic!("{name}: decode: {e}"));
            assert_eq!(got, c, "{name}: round trip");
        }
    }

    /// Go's invalid-UTF-8 label case is unrepresentable here: `String` is
    /// UTF-8 by construction. `decode_rejects_jj_fields` covers it on the wire.
    #[test]
    fn encode_rejects_invalid_jj_fields() {
        let ka = root("a").0;
        let blob = blob_key();
        let cases: [(Mutation, String); 16] = [
            (
                ("one term", |c| {
                    c.conflict_terms.truncate(1);
                    c.conflict_labels = Vec::new();
                }),
                "commit has 1 conflict terms, want an even number up to 254: a remove for every further add".into(),
            ),
            (
                ("too many terms", |c| {
                    c.conflict_terms = dir_keys(MAX_CONFLICT_TERMS + 2);
                    c.conflict_labels = Vec::new();
                }),
                "commit has 256 conflict terms, want an even number up to 254: a remove for every further add".into(),
            ),
            (
                ("term is a blob", |c| c.conflict_terms[0] = blob_key()),
                format!("commit conflict term 0: {blob} is not a directory key (type Blob)"),
            ),
            (
                ("term is a commit", |c| c.conflict_terms[1] = root("a").0),
                format!("commit conflict term 1: {ka} is not a directory key (type Commit)"),
            ),
            (
                ("non-canonical term", |c| c.conflict_terms[0].0[0] |= 0x08),
                "commit conflict term 0: key: reserved header bit is set".into(),
            ),
            (
                ("labels without terms", |c| c.conflict_terms = Vec::new()),
                "commit has conflict labels but no conflict".into(),
            ),
            // The count is right: only the rule against labels on a resolved
            // tree rejects it.
            (
                ("one label, no terms", |c| {
                    c.conflict_terms = Vec::new();
                    c.conflict_labels = vec!["ours".into()];
                }),
                "commit has conflict labels but no conflict".into(),
            ),
            (
                ("a label too few", |c| c.conflict_labels.truncate(2)),
                "commit has 2 conflict labels for 3 terms".into(),
            ),
            (
                ("a label too many", |c| c.conflict_labels.push("x".into())),
                "commit has 4 conflict labels for 3 terms".into(),
            ),
            (
                ("all labels empty", |c| {
                    c.conflict_labels = vec![String::new(); 3]
                }),
                "commit conflict labels are all empty; an unlabelled conflict carries none".into(),
            ),
            (
                ("label too long", |c| {
                    c.conflict_labels[0] = "l".repeat(MAX_LABEL_LEN + 1)
                }),
                "commit conflict label 0 exceeds 65536 bytes".into(),
            ),
            (
                ("control char in label", |c| {
                    c.conflict_labels[0] = "ou\nrs".into()
                }),
                "commit conflict label 0 must not contain control characters".into(),
            ),
            (
                ("change id too long", |c| {
                    c.change_id = vec![0; MAX_CHANGE_ID_LEN + 1]
                }),
                "commit change id exceeds 64 bytes".into(),
            ),
            // The check order is Go's: the change id before the terms, the
            // number of terms before any one of them, the label count before
            // the labels' text, a label's length before its characters.
            (
                ("order: change id, then terms", |c| {
                    c.change_id = vec![0; MAX_CHANGE_ID_LEN + 1];
                    c.conflict_terms.truncate(1);
                }),
                "commit change id exceeds 64 bytes".into(),
            ),
            (
                ("order: term count, then the term", |c| {
                    c.conflict_terms = vec![blob_key()];
                    c.conflict_labels = Vec::new();
                }),
                "commit has 1 conflict terms, want an even number up to 254: a remove for every further add".into(),
            ),
            (
                ("order: label count, then the text", |c| {
                    c.conflict_labels = vec!["a\nb".into(), "x".into()]
                }),
                "commit has 2 conflict labels for 3 terms".into(),
            ),
        ];
        for ((name, mutate), want) in cases {
            let mut c = conflicted();
            mutate(&mut c);
            let err = c.encode().expect_err(name);
            assert_eq!(err.to_string(), want, "{name}");
            assert_eq!(c.object().expect_err(name), err, "{name}: object");
        }
    }

    /// Go's `TestDecodeRejectsNonCanonicalJJFields`, plus the strict-decoding
    /// rejections of keys 7–9 with the diagnostics Go produces for them: the
    /// decode stage's are fxamacker's, and a type error inside an array names
    /// the element's Go type under the array's field.
    #[test]
    fn decode_rejects_jj_fields() {
        let good = hand_conflicted();
        Commit::decode(&good).expect("baseline must decode");
        let c = conflicted();
        let ka = c.parents[0];
        let (t0, t1) = (bstr32(c.conflict_terms[0]), bstr32(c.conflict_terms[1]));
        let change_id = [vec![0x07, 0x50], CHANGE_ID.to_vec()].concat();
        let terms = [vec![0x08, 0x82], t0.clone(), t1.clone()].concat();
        let labels = [vec![0x09, 0x83], tstr("ours"), tstr(""), tstr("theirs")].concat();
        let resolved = hand_merge();
        // one more map entry
        let grown = |b: &[u8], extra: &[u8]| {
            let mut out = [b, extra].concat();
            out[0] += 1;
            out
        };
        let with_terms = |v: Vec<u8>| replace_once(&good, &terms, &[vec![0x08], v].concat());
        let with_labels = |v: Vec<u8>| replace_once(&good, &labels, &[vec![0x09], v].concat());
        let with_id = |v: Vec<u8>| replace_once(&good, &change_id, &[vec![0x07], v].concat());
        const NOT_CANONICAL: &str = "commit encoding is not canonical";
        const FIELD: &str = "decoding commit: cbor: cannot unmarshal";
        let cases: Vec<(&str, Vec<u8>, String)> = vec![
            // Absence has one encoding.
            ("empty terms", grown(&resolved, &[0x08, 0x80]), NOT_CANONICAL.into()),
            ("empty change id", grown(&resolved, &[0x07, 0x40]), NOT_CANONICAL.into()),
            ("empty labels", with_labels(vec![0x80]), NOT_CANONICAL.into()),
            (
                "labels before terms",
                replace_once(
                    &good,
                    &[terms.clone(), labels.clone()].concat(),
                    &[labels.clone(), terms.clone()].concat(),
                ),
                NOT_CANONICAL.into(),
            ),
            (
                "labels, no terms",
                {
                    let mut b = replace_once(&good, &terms, &[]);
                    b[0] -= 1;
                    b
                },
                "invalid commit: commit has conflict labels but no conflict".into(),
            ),
            (
                "terms null",
                with_terms(vec![0xf6]),
                "invalid commit: commit has conflict labels but no conflict".into(),
            ),
            (
                "one term",
                with_terms([vec![0x81], t0.clone()].concat()),
                "invalid commit: commit has 1 conflict terms, want an even number up to 254: a remove for every further add".into(),
            ),
            (
                "term is a commit",
                replace_once(&good, &t1, &bstr32(ka)),
                format!("invalid commit: commit conflict term 1: {ka} is not a directory key (type Commit)"),
            ),
            (
                "tree is a commit",
                replace_once(
                    &good,
                    &[vec![0x00], bstr32(empty_dir())].concat(),
                    &[vec![0x00], bstr32(ka)].concat(),
                ),
                format!("invalid commit: commit tree {ka} is not a directory key (type Commit)"),
            ),
            (
                "short term",
                with_terms(
                    [
                        vec![0x82, 0x58, 0x1f],
                        c.conflict_terms[0].as_bytes()[..31].to_vec(),
                        t1.clone(),
                    ]
                    .concat(),
                ),
                "invalid commit: conflict term 0: key: data is not 32 bytes: got 31".into(),
            ),
            (
                "indefinite terms",
                with_terms([vec![0x9f], t0.clone(), t1.clone(), vec![0xff]].concat()),
                NOT_CANONICAL.into(),
            ),
            (
                "non-minimal change id head",
                with_id([vec![0x58, 0x10], CHANGE_ID.to_vec()].concat()),
                NOT_CANONICAL.into(),
            ),
            (
                "65-byte change id",
                with_id([vec![0x58, 0x41], vec![7; 65]].concat()),
                "invalid commit: commit change id exceeds 64 bytes".into(),
            ),
            // The first occurrence of a key wins; the second is skipped
            // without a look at its type.
            ("duplicate key 7", grown(&good, &[0x07, 0x41, 0x00]), NOT_CANONICAL.into()),
            ("unknown key 10", grown(&good, &[0x0a, 0x00]), NOT_CANONICAL.into()),
            (
                "trailing byte",
                [good.clone(), vec![0x00]].concat(),
                "decoding commit: cbor: 1 bytes of extraneous data starting at index 258".into(),
            ),
            (
                "change id is text",
                with_id(tstr("ours")),
                format!("{FIELD} UTF-8 text string into Go struct field commit.wireCommit.7 of type []uint8"),
            ),
            (
                "terms is a byte string",
                with_terms(t0.clone()),
                format!("{FIELD} byte string into Go struct field commit.wireCommit.8 of type [][]uint8"),
            ),
            (
                "a term is a number",
                with_terms([vec![0x82], t0.clone(), vec![0x07]].concat()),
                format!("{FIELD} positive integer into Go struct field commit.wireCommit.8 of type []uint8"),
            ),
            (
                "labels is a text string",
                with_labels(tstr("ours")),
                format!("{FIELD} UTF-8 text string into Go struct field commit.wireCommit.9 of type []string"),
            ),
            (
                "labels is a map",
                with_labels(vec![0xa0]),
                format!("{FIELD} map into Go struct field commit.wireCommit.9 of type []string"),
            ),
            (
                "a label is bytes",
                with_labels([vec![0x83], tstr("ours"), vec![0x41, 0x00], tstr("theirs")].concat()),
                format!("{FIELD} byte string into Go struct field commit.wireCommit.9 of type string"),
            ),
            (
                "a label is a number",
                with_labels([vec![0x83], tstr("ours"), vec![0x07], tstr("theirs")].concat()),
                format!("{FIELD} positive integer into Go struct field commit.wireCommit.9 of type string"),
            ),
            // null leaves the zero value, the empty label, behind.
            (
                "a label is null",
                with_labels([vec![0x83], tstr("ours"), vec![0xf6], tstr("theirs")].concat()),
                NOT_CANONICAL.into(),
            ),
            (
                "label with invalid UTF-8",
                with_labels([vec![0x83, 0x62, 0xc3, 0x28], tstr(""), tstr("theirs")].concat()),
                "decoding commit: cbor: invalid UTF-8 string".into(),
            ),
            (
                "control char in label",
                with_labels([vec![0x83], tstr("a\nb"), tstr(""), tstr("theirs")].concat()),
                "invalid commit: commit conflict label 0 must not contain control characters".into(),
            ),
            (
                "DEL in the last label",
                with_labels([vec![0x83], tstr("ours"), tstr(""), tstr("\x7f")].concat()),
                "invalid commit: commit conflict label 2 must not contain control characters".into(),
            ),
            (
                "a label too few",
                with_labels([vec![0x82], tstr("ours"), tstr("")].concat()),
                "invalid commit: commit has 2 conflict labels for 3 terms".into(),
            ),
            (
                "all labels empty",
                with_labels(vec![0x83, 0x60, 0x60, 0x60]),
                "invalid commit: commit conflict labels are all empty; an unlabelled conflict carries none".into(),
            ),
        ];
        for (name, b, want) in cases {
            let err = Commit::decode(&b).expect_err(name);
            assert_eq!(err.to_string(), want, "{name}");
        }
    }

    #[test]
    fn signature_payload_covers_new_fields() {
        let base = conflicted().signature_payload().unwrap();
        let cases: [Mutation; 3] = [
            ("change id", |c| c.change_id = vec![9]),
            ("a label", |c| c.conflict_labels[1] = "base".into()),
            ("a term", |c| c.conflict_terms.swap(0, 1)),
        ];
        for (name, mutate) in cases {
            let mut c = conflicted();
            mutate(&mut c);
            assert_ne!(
                c.signature_payload().unwrap(),
                base,
                "the signature payload does not cover {name}"
            );
        }
    }

    /// The key's length is a footprint, as a directory's is: the commit's own
    /// bytes plus each of its trees. Parents are not counted.
    #[test]
    fn object_length_is_the_footprint() {
        let (k, b) = conflicted().object().unwrap();
        assert_eq!(
            k.length(),
            b.len() as u64 + 1 + 300 + 70000,
            "own bytes and every term"
        );
        // A parent's length, however large, changes nothing.
        let mut c = conflicted();
        let mut h = [0u8; key::SIZE];
        h[0] = 7;
        c.parents = vec![Key::new_from_hash(Type::Commit, 1 << 40, h)];
        let (k2, b2) = c.object().unwrap();
        assert_eq!(
            k2.length(),
            b2.len() as u64 + 1 + 300 + 70000,
            "parents must not be counted"
        );
    }

    #[test]
    fn object_rejects_length_overflow() {
        let mut c = conflicted();
        c.conflict_labels = Vec::new();
        c.conflict_terms = vec![
            dir_key(Type::DirNode, u64::MAX, 0x33),
            dir_key(Type::DirLeaf, 5, 0x44),
        ];
        let err = c
            .object()
            .expect_err("trees whose lengths overflow 64 bits");
        assert_eq!(err, Error::FootprintOverflow);
        assert_eq!(
            err.to_string(),
            "commit footprint overflows the key's length field"
        );
        // The record itself is sound: only the key cannot be made.
        let own = c.encode().expect("encode has no opinion").len() as u64;

        // The edge: a sum of exactly 2^64 - 1 fits, one more does not.
        let rest = own + c.tree.length() + 5;
        c.conflict_terms[0] = dir_key(Type::DirNode, u64::MAX - rest, 0x33);
        assert_eq!(c.object().unwrap().0.length(), u64::MAX);
        c.conflict_terms[0] = dir_key(Type::DirNode, u64::MAX - rest + 1, 0x33);
        assert_eq!(c.object(), Err(Error::FootprintOverflow));
    }

    #[test]
    fn footprint_sums_own_bytes_and_trees() {
        let (a, b) = (
            dir_key(Type::DirLeaf, 300, 1),
            dir_key(Type::DirNode, 70000, 2),
        );
        assert_eq!(footprint(0, &[]), Ok(0));
        assert_eq!(footprint(258, &[empty_dir(), a, b]), Ok(70559));
        assert_eq!(footprint(u64::MAX, &[]), Ok(u64::MAX));
        assert_eq!(
            footprint(u64::MAX - 1, &[empty_dir()]),
            Ok(u64::MAX),
            "exactly fitting"
        );
        assert_eq!(
            footprint(u64::MAX, &[empty_dir()]),
            Err(Error::FootprintOverflow)
        );
        assert_eq!(
            footprint(
                0,
                &[
                    dir_key(Type::DirNode, 1 << 63, 1),
                    dir_key(Type::DirNode, 1 << 63, 2)
                ]
            ),
            Err(Error::FootprintOverflow)
        );
    }

    // The second golden vector: conflicted(), pinned like the first. Tree the
    // empty directory; conflict terms Key::new_from_hash(DirLeaf, 300, 0x11…)
    // and Key::new_from_hash(DirNode, 70000, 0x22…); labels "ours", "",
    // "theirs"; change id 00 01 … 0f; parent the root commit "a"; author Ann;
    // committer without a name, bot@example.com, at Bob's time and zone;
    // message "conflict\n". 258 bytes; length field 258+1+300+70000 = 70559.
    const GOLDEN_CONFLICTED_KEY: &str =
        "5201139f190aa234ad713392987671d47e7e3292a613c62e3d14a85d614b300b";
    const GOLDEN_CONFLICTED_BYTES: &str = "a80058202001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b018158205074d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c02a40063416e6e016f616e6e406578616d706c652e636f6d021b17979cfe362a000003187803a40060016f626f74406578616d706c652e636f6d021b17979cfe362a00010339012b0469636f6e666c6963740a0750000102030405060708090a0b0c0d0e0f0882582021012c1111111111111111111111111111111111111111111111111111111111582032011170222222222222222222222222222222222222222222222222222222220983646f7572736066746865697273";

    #[test]
    fn golden_vector_conflicted() {
        let (k, b) = conflicted().object().unwrap();
        assert_eq!(hex::encode(&b), GOLDEN_CONFLICTED_BYTES);
        assert_eq!(b.len(), 258);
        assert_eq!(k.to_string(), GOLDEN_CONFLICTED_KEY);
        assert_eq!(k.length(), 70559);
        let decoded = Commit::decode(&hex::decode(GOLDEN_CONFLICTED_BYTES).unwrap()).unwrap();
        assert_eq!(decoded, conflicted());
    }
}
