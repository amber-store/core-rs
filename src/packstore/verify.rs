//! Store scrub: full CRC + hash verification of sealed segments (Go:
//! `packstore/verify.go`).

use std::sync::Arc;

use crate::amberpack::{REC_HEADER_SIZE, decode_payload, parse_record};
use crate::commit::{self, Commit};
use crate::key::{Key, Type};

use super::footer::{IndexEntry, SealedSegment, build_index_section, filter_key};
use super::{Error, MAGIC_HEADER, Store, unpoison};

impl Store {
    /// Scrubs every sealed segment: walks the body record by record
    /// (validating framing, CRCs, and that each payload re-hashes to its
    /// key), recomputes the index section and compares it bytewise with the
    /// footer's, and checks the filter contains every body key. The active
    /// segment is covered by tail-scan on reopen, not by `verify`. Segments
    /// sealed by rotations that happen after `verify` snapshots the segment
    /// list are not covered by that call. The snapshot keeps its mappings
    /// alive independently, so a concurrent [`Store::close`] or
    /// [`Store::wipe`] is safe; `cancel` returning `true` stops the scrub
    /// early with [`Error::Canceled`] (Go: `Verify(ctx)`).
    pub fn verify(&self, cancel: impl Fn() -> bool) -> Result<(), Error> {
        let segs: Vec<Arc<SealedSegment>> = {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return Err(Error::Closed);
            }
            sh.sealed.clone()
        };
        for seg in segs {
            seg.verify(&cancel)?;
        }
        Ok(())
    }
}

impl SealedSegment {
    /// Scrubs one sealed segment. The segment is immutable and the caller
    /// holds no locks: scrubbing runs concurrently with reads and writes
    /// (Go: `sealedSegment.verify`).
    pub(crate) fn verify(&self, cancel: &dyn Fn() -> bool) -> Result<(), Error> {
        let path = self.path.display();
        let body_len = self.fv.body_len as usize;
        let mut entries: Vec<IndexEntry> = Vec::new();
        let mut off = MAGIC_HEADER.len();
        while off < body_len {
            if cancel() {
                return Err(Error::Canceled);
            }
            let rec = parse_record(&self.mm[off..body_len]).map_err(|e| Error::Corrupt {
                msg: format!("{path}: record at offset {off}: {e}"),
                verify: false,
            })?;
            let payload_at = off + REC_HEADER_SIZE;
            let payload = decode_payload(
                rec.flags,
                rec.ulen,
                &self.mm[payload_at..payload_at + rec.slen as usize],
            )
            .map_err(|e| Error::Corrupt {
                msg: format!("{path}: record at offset {off}: {e}"),
                verify: false,
            })?;
            if let Err(msg) = verify_object(rec.key, &payload) {
                // Scrub findings are corruption: both classes match, exactly
                // like Go's double `%w` wrap of ErrCorrupt and ErrVerify.
                return Err(Error::Corrupt {
                    msg: format!(
                        "amberpack: corrupt pack data: {path}: record at offset {off}: {msg}"
                    ),
                    verify: true,
                });
            }
            if !self.fv.filter.contains(filter_key(rec.key)) {
                return Err(Error::Corrupt {
                    msg: format!(
                        "amberpack: corrupt pack data: {path}: filter missing key {} (offset {off})",
                        rec.key
                    ),
                    verify: false,
                });
            }
            entries.push(IndexEntry {
                k: rec.key,
                off: off as u64,
                slen: rec.slen,
            });
            off += REC_HEADER_SIZE + rec.slen as usize;
        }
        // Defensive only: parse_record bounds every record against body_len,
        // so the walk can only exit exactly at body_len or via an error above.
        if off != body_len {
            return Err(Error::Corrupt {
                msg: format!(
                    "amberpack: corrupt pack data: {path}: records end at {off}, trailer says {body_len}"
                ),
                verify: false,
            });
        }
        if entries.len() as u64 != self.fv.key_count {
            return Err(Error::Corrupt {
                msg: format!(
                    "amberpack: corrupt pack data: {path}: body has {} records, trailer says {}",
                    entries.len(),
                    self.fv.key_count
                ),
                verify: false,
            });
        }
        let rebuilt = build_index_section(&entries);
        let stored =
            &self.mm[self.fv.index_off as usize..(self.fv.index_off + self.fv.index_len) as usize];
        if rebuilt != stored {
            return Err(Error::Corrupt {
                msg: format!(
                    "amberpack: corrupt pack data: {path}: index section does not match body"
                ),
                verify: false,
            });
        }
        Ok(())
    }
}

/// Recomputes `k` from `data` and reports a verification-failure message on
/// mismatch (the complete `ErrVerify`-prefixed text Go produces). For Blob
/// and XattrSet, whose key length is the serialized byte length, it also
/// checks the length field; and for Commit, whose length is a footprint — own
/// bytes plus the trees it records — which the commit's own bytes suffice to
/// recompute. A commit that does not decode fails verification. The other
/// aggregate types (FileNode/DirLeaf/DirNode) carry a logical length the
/// store does not recompute, so only their hash is checked (Go:
/// `verifyObject`).
pub(crate) fn verify_object(k: Key, data: &[u8]) -> Result<(), String> {
    let sum = *blake3::hash(data).as_bytes();
    let raw_type = k.as_bytes()[0] >> 4;
    let Some(t) = Type::from_u8(raw_type) else {
        // Go: key.NewFromHash rejects the reserved type nibble.
        return Err(format!(
            "packstore: object verification failed: {k}: key: reserved object type: {raw_type}"
        ));
    };
    let want = Key::new_from_hash(t, k.length(), sum);
    if want != k {
        return Err(format!(
            "packstore: object verification failed: payload hashes to {want}, not {k}"
        ));
    }
    match t {
        Type::Blob | Type::XattrSet => {
            if k.length() != data.len() as u64 {
                return Err(format!(
                    "packstore: object verification failed: {k} length field {} != payload {}",
                    k.length(),
                    data.len()
                ));
            }
        }
        Type::Commit => {
            let c = Commit::decode(data)
                .map_err(|e| format!("packstore: object verification failed: {k}: {e}"))?;
            let want = commit::footprint(data.len() as u64, &c.trees())
                .map_err(|e| format!("packstore: object verification failed: {k}: {e}"))?;
            if k.length() != want {
                return Err(format!(
                    "packstore: object verification failed: {k} length field {} != footprint {want} (own {} bytes plus its trees)",
                    k.length(),
                    data.len()
                ));
            }
        }
        Type::FileNode | Type::DirLeaf | Type::DirNode => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify_object;
    use crate::commit::{Commit, Identity};
    use crate::key::{Key, SIZE, Type};

    /// Port of Go `TestVerifyObjectChecksCommitLength`. A commit's length
    /// field is a footprint: its own bytes plus every tree it records. The
    /// store checks it, decoding the commit to learn the trees.
    #[test]
    fn verify_object_checks_commit_length() {
        const PREFIX: &str = "packstore: object verification failed:";
        let tree = Key::new(Type::DirLeaf, 1, &[0x80]);
        let id = Identity {
            name: "Ann".into(),
            when: 1,
            ..Default::default()
        };
        let resolved = Commit {
            tree,
            parents: Vec::new(),
            author: id.clone(),
            committer: id,
            message: "m".into(),
            signature: Vec::new(),
            public_key: Vec::new(),
            change_id: Vec::new(),
            conflict_terms: Vec::new(),
            conflict_labels: Vec::new(),
        };
        let (good, data) = resolved.object().unwrap();
        assert_eq!(verify_object(good, &data), Ok(()), "an honest commit key");
        // The rule of the first release: own bytes only. Same hash, wrong length.
        let own = data.len();
        let own_only = Key::new(Type::Commit, own as u64, &data);
        assert_eq!(
            verify_object(own_only, &data),
            Err(format!(
                "{PREFIX} {own_only} length field {own} != footprint {} (own {own} bytes plus its trees)",
                own + 1
            )),
            "a length that leaves the tree out"
        );

        let mut h = [0u8; SIZE];
        h[0] = 1;
        let remove = Key::new_from_hash(Type::DirLeaf, 300, h);
        h[0] = 2;
        let add = Key::new_from_hash(Type::DirNode, 70000, h);
        let mut conflicted = resolved.clone();
        conflicted.conflict_terms = vec![remove, add];
        let (good_c, data_c) = conflicted.object().unwrap();
        assert_eq!(
            verify_object(good_c, &data_c),
            Ok(()),
            "an honest conflicted commit key"
        );
        let own = data_c.len();
        let first_tree_only = Key::new(Type::Commit, own as u64 + tree.length(), &data_c);
        assert_eq!(
            verify_object(first_tree_only, &data_c),
            Err(format!(
                "{PREFIX} {first_tree_only} length field {} != footprint {} (own {own} bytes plus its trees)",
                own + 1,
                own + 1 + 300 + 70000
            )),
            "a length that leaves conflict terms out"
        );

        // Bytes that are no commit fail verification, whatever their length.
        let junk = b"not a commit";
        let k = Key::new(Type::Commit, junk.len() as u64, junk);
        assert_eq!(
            verify_object(k, junk),
            Err(format!("{PREFIX} {k}: decoding commit: unexpected EOF"))
        );

        // A record that decodes but whose footprint no key can carry.
        conflicted.conflict_terms = vec![Key::new_from_hash(Type::DirNode, u64::MAX, h), add];
        let data_h = conflicted.encode().unwrap();
        let k = Key::new(Type::Commit, data_h.len() as u64, &data_h);
        assert_eq!(
            verify_object(k, &data_h),
            Err(format!(
                "{PREFIX} {k}: commit footprint overflows the key's length field"
            ))
        );
    }
}
