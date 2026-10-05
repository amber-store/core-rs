//! Name lookups in DirLeaf and DirNode bodies, without decoding them.
//!
//! A scan answers only for the canonical form the encoder writes. Anything
//! else is `Unsure` and goes to the full decoder, so a malformed object still
//! fails with the error Go would give.

use std::ops::Range;

/// fxamacker's default `MaxArrayElements` and `MaxMapPairs`.
const MAX_ITEMS: u64 = 131072;

pub(super) enum Leaf {
    /// The byte range of the one entry map with that name.
    Found(Range<usize>),
    Missing,
    Unsure,
}

pub(super) enum Node<'a> {
    /// The child key of the first pair whose separator is not below the name.
    Child(&'a [u8]),
    Missing,
    Unsure,
}

struct Cur<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Cur<'a> {
    fn head(&mut self) -> Option<(u8, u64)> {
        let ib = *self.b.get(self.off)?;
        self.off += 1;
        let (major, ai) = (ib >> 5, ib & 0x1f);
        let (n, min) = match ai {
            0..=23 => return Some((major, u64::from(ai))),
            24 => (1, 24),
            25 => (2, 1 << 8),
            26 => (4, 1 << 16),
            27 => (8, 1 << 32),
            _ => return None,
        };
        let arg = self.b.get(self.off..self.off + n)?;
        self.off += n;
        let v = arg.iter().fold(0u64, |a, &x| a << 8 | u64::from(x));
        (v >= min).then_some((major, v))
    }

    fn uint(&mut self) -> Option<u64> {
        match self.head()? {
            (0, v) => Some(v),
            _ => None,
        }
    }

    fn int(&mut self) -> Option<()> {
        match self.head()? {
            (0 | 1, v) if v <= i64::MAX as u64 => Some(()),
            _ => None,
        }
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let (major, n) = self.head()?;
        if major != 2 {
            return None;
        }
        let end = self.off.checked_add(usize::try_from(n).ok()?)?;
        let s = self.b.get(self.off..end)?;
        self.off = end;
        Some(s)
    }

    fn uints(&mut self) -> Option<()> {
        match self.head()? {
            (4, n) if n <= MAX_ITEMS => (0..n).try_for_each(|_| self.uint().map(drop)),
            _ => None,
        }
    }

    fn array(&mut self) -> Option<u64> {
        match self.head()? {
            (4, n) if n <= MAX_ITEMS => Some(n),
            _ => None,
        }
    }

    fn done(&self) -> bool {
        self.off == self.b.len()
    }
}

pub(super) fn leaf(b: &[u8], name: &[u8]) -> Leaf {
    scan_leaf(b, name).unwrap_or(Leaf::Unsure)
}

fn scan_leaf(b: &[u8], name: &[u8]) -> Option<Leaf> {
    let mut c = Cur { b, off: 0 };
    let n = c.array()?;
    let mut prev: Option<&[u8]> = None;
    let mut found = None;
    for _ in 0..n {
        let start = c.off;
        match c.head()? {
            (5, pairs) if (5..=10).contains(&pairs) => {
                if c.uint()? != 0 {
                    return None;
                }
                let ename = c.bytes()?;
                if prev.is_some_and(|p| p >= ename) {
                    return None;
                }
                prev = Some(ename);
                let mut last = 0;
                for _ in 1..pairs {
                    let k = c.uint()?;
                    if k <= last {
                        return None;
                    }
                    last = k;
                    match k {
                        1..=3 => c.uint().map(drop)?,
                        4 => c.int()?,
                        5 | 6 | 9 => c.bytes().map(drop)?,
                        7 => c.uints()?,
                        // Inline xattrs are raw CBOR the decoder re-validates.
                        _ => return None,
                    }
                }
                if ename == name {
                    found = Some(start..c.off);
                }
            }
            _ => return None,
        }
    }
    if !c.done() {
        return None;
    }
    Some(found.map_or(Leaf::Missing, Leaf::Found))
}

pub(super) fn node<'a>(b: &'a [u8], name: &[u8]) -> Node<'a> {
    scan_node(b, name).unwrap_or(Node::Unsure)
}

fn scan_node<'a>(b: &'a [u8], name: &[u8]) -> Option<Node<'a>> {
    let mut c = Cur { b, off: 0 };
    let n = c.array()?;
    let mut prev: Option<&[u8]> = None;
    let mut found = None;
    for _ in 0..n {
        if c.head()? != (4, 2) {
            return None;
        }
        let sep = c.bytes()?;
        let child = c.bytes()?;
        if prev.is_some_and(|p| p >= sep) {
            return None;
        }
        prev = Some(sep);
        if found.is_none() && sep >= name {
            found = Some(child);
        }
    }
    if !c.done() {
        return None;
    }
    Some(found.map_or(Node::Missing, Node::Child))
}

#[cfg(test)]
mod tests {
    use super::super::encode::{marshal_entries, marshal_pairs};
    use super::super::{DirPair, Entry, decode_dir_leaf, decode_dir_node};
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn bytes(&mut self, len: usize, alphabet: &[u8]) -> Vec<u8> {
            (0..len)
                .map(|_| alphabet[self.below(alphabet.len() as u64) as usize])
                .collect()
        }
        fn big(&mut self) -> u64 {
            match self.below(4) {
                0 => self.below(24),
                1 => self.below(1 << 16),
                2 => self.below(1 << 40),
                _ => self.next(),
            }
        }
    }

    fn names(r: &mut Rng, n: usize) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = (0..n)
            .map(|_| {
                let len = 1 + r.below(12) as usize;
                r.bytes(len, b"abcxyz._-/\x00\xff")
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn entry(r: &mut Rng, name: Vec<u8>) -> Entry {
        let mut e = Entry {
            name,
            mode: [0o100644, 0o100755, 0o040755, 0o120777, 0o020644][r.below(5) as usize],
            uid: r.big(),
            gid: r.big(),
            mtime: r.next() as i64,
            ..Entry::default()
        };
        if r.below(4) != 0 {
            e.content_key = r.bytes(32, &[0, 1, 2, 0xff]);
        }
        if r.below(4) == 0 {
            let len = 1 + r.below(40) as usize;
            e.link_target = r.bytes(len, b"ab/.");
        }
        if r.below(8) == 0 {
            e.rdev = vec![r.big(), r.big()];
        }
        if r.below(8) == 0 {
            e.xattrs_key = r.bytes(32, &[7, 8]);
        }
        e
    }

    fn slow_leaf(b: &[u8], name: &[u8]) -> Result<Option<Entry>, ()> {
        let entries = decode_dir_leaf(b).map_err(drop)?;
        let i = entries.partition_point(|e| e.name.as_slice() < name);
        Ok(entries.get(i).filter(|e| e.name == name).cloned())
    }

    fn slow_node(b: &[u8], name: &[u8]) -> Result<Option<Vec<u8>>, ()> {
        let pairs = decode_dir_node(b).map_err(drop)?;
        let i = pairs.partition_point(|p| p.sep_name.as_slice() < name);
        Ok(pairs.get(i).map(|p| p.child_key.clone()))
    }

    /// False when the scan defers; otherwise asserts the decoder agrees.
    fn agrees_leaf(b: &[u8], name: &[u8]) -> bool {
        match leaf(b, name) {
            Leaf::Found(span) => {
                let mut one = vec![0x81];
                one.extend_from_slice(&b[span]);
                let e = decode_dir_leaf(&one).unwrap();
                assert_eq!(Ok(Some(e[0].clone())), slow_leaf(b, name));
                true
            }
            Leaf::Missing => {
                assert_eq!(Ok(None), slow_leaf(b, name));
                true
            }
            Leaf::Unsure => false,
        }
    }

    fn agrees_node(b: &[u8], name: &[u8]) -> bool {
        match node(b, name) {
            Node::Child(c) => {
                assert_eq!(Ok(Some(c.to_vec())), slow_node(b, name));
                true
            }
            Node::Missing => {
                assert_eq!(Ok(None), slow_node(b, name));
                true
            }
            Node::Unsure => false,
        }
    }

    fn mutations(r: &mut Rng, b: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for _ in 0..24 {
            let mut m = b.to_vec();
            if !m.is_empty() {
                let i = r.below(m.len() as u64) as usize;
                m[i] ^= 1 << r.below(8);
            }
            out.push(m);
        }
        for _ in 0..4 {
            out.push(b[..r.below(b.len() as u64 + 1) as usize].to_vec());
        }
        let mut longer = b.to_vec();
        longer.push(0);
        out.push(longer);
        out
    }

    #[test]
    fn leaf_agrees_with_decoder() {
        let mut r = Rng(0x9e3779b97f4a7c15);
        for _ in 0..500 {
            let n = r.below(40) as usize;
            let entries: Vec<Entry> = names(&mut r, n)
                .into_iter()
                .map(|name| entry(&mut r, name))
                .collect();
            let b = marshal_entries(&entries).unwrap();
            let mut queries: Vec<Vec<u8>> = entries.iter().map(|e| e.name.clone()).collect();
            queries.extend(names(&mut r, 4));
            queries.push(Vec::new());
            for q in &queries {
                assert!(agrees_leaf(&b, q), "canonical leaf left to the decoder");
            }
            for m in mutations(&mut r, &b) {
                for q in &queries {
                    agrees_leaf(&m, q);
                }
            }
        }
    }

    #[test]
    fn node_agrees_with_decoder() {
        let mut r = Rng(0x2545f4914f6cdd1d);
        for _ in 0..500 {
            let n = r.below(40) as usize;
            let pairs: Vec<DirPair> = names(&mut r, n)
                .into_iter()
                .map(|sep_name| DirPair {
                    sep_name,
                    child_key: r.bytes(32, &[3, 4, 5]),
                })
                .collect();
            let b = marshal_pairs(&pairs);
            let mut queries: Vec<Vec<u8>> = pairs.iter().map(|p| p.sep_name.clone()).collect();
            queries.extend(names(&mut r, 4));
            queries.push(Vec::new());
            for q in &queries {
                assert!(agrees_node(&b, q), "canonical node left to the decoder");
            }
            for m in mutations(&mut r, &b) {
                for q in &queries {
                    agrees_node(&m, q);
                }
            }
        }
    }

    #[test]
    fn leaf_defers_inline_xattrs_and_unsorted_names() {
        let a = Entry {
            name: b"a".to_vec(),
            xattrs_in: vec![0xa1, 0x41, b'k', 0x41, b'v'],
            ..Entry::default()
        };
        let b = marshal_entries(std::slice::from_ref(&a)).unwrap();
        assert!(matches!(leaf(&b, b"a"), Leaf::Unsure));

        let z = Entry {
            name: b"z".to_vec(),
            ..Entry::default()
        };
        let y = Entry {
            name: b"y".to_vec(),
            ..Entry::default()
        };
        let b = marshal_entries(&[z, y]).unwrap();
        assert!(matches!(leaf(&b, b"y"), Leaf::Unsure));
    }
}
