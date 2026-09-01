//! Working out what to send, before sending it.
//!
//! Two stores want to end up agreeing about a subgraph. The receiver could
//! list everything it has, but that list is the size of the repository and the
//! answer is usually "almost all of it". So the receiver sends a summary
//! instead, the sender walks the graph against it, and only the difference
//! moves.
//!
//! # A summary that can be wrong in one direction only
//!
//! A Bloom filter can say "no" with certainty and "yes" with a probability. In
//! this direction that is exactly the right shape: a false positive means the
//! sender believes the receiver has something it does not, which would leave a
//! gap. So a probabilistic hit never prunes. It is recorded, the walk carries
//! on underneath it, and the receiver is asked to confirm — one round trip
//! against the alternative, which is an incomplete transfer that looks
//! complete.
//!
//! The other error a summary can make — claiming *not* to have something it
//! does — costs bytes and nothing else. A Bloom filter cannot make that one.

use std::collections::BTreeSet;

use crate::error::{ObjectError, Result};
use crate::id::ObjectId;
use crate::varint::{read_varuint, write_varuint};
use crate::walk::HaveSet;

/// Protocol version this build speaks.
pub const PROTOCOL_V2: u16 = 2;

/// What each side can do, exchanged before anything large moves.
///
/// Negotiating first is what lets the format change later without a flag day:
/// a new sender talking to an old receiver finds out before it has committed
/// to an encoding, rather than after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub protocol_versions: Vec<u16>,
    pub object_formats: Vec<u16>,
    pub bundle_formats: Vec<u16>,
    /// Have-set forms this side will accept from the other.
    pub have_forms: Vec<HaveForm>,
    /// Whether this side can serve a walk restricted by object kind.
    pub filters: bool,
    /// Whether a transfer can be resumed rather than restarted.
    pub resumable: bool,
    /// Largest number of roots one request may ask for. A request naming a
    /// million roots is a request to walk the whole store on someone else's
    /// hardware.
    pub max_roots: usize,
    /// Largest number of objects one response will carry.
    pub max_objects_per_response: usize,
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities {
            protocol_versions: vec![PROTOCOL_V2],
            object_formats: vec![crate::envelope::ENVELOPE_FORMAT_V1],
            bundle_formats: vec![crate::bundle::BUNDLE_FORMAT_V1],
            have_forms: vec![HaveForm::Exact, HaveForm::Bloom],
            filters: true,
            resumable: true,
            max_roots: 1024,
            max_objects_per_response: 100_000,
        }
    }
}

impl Capabilities {
    /// What the two sides can both do, or why they cannot talk.
    pub fn agree(&self, other: &Capabilities) -> Result<Agreement> {
        let protocol = highest_common(&self.protocol_versions, &other.protocol_versions).ok_or(
            ObjectError::Corrupt(format!(
                "no protocol version in common: this side speaks {:?}, the other {:?}",
                self.protocol_versions, other.protocol_versions
            )),
        )?;
        let object_format = highest_common(&self.object_formats, &other.object_formats).ok_or(
            ObjectError::Corrupt("no object format in common".to_string()),
        )?;
        // Prefer exact: it prunes, and it never needs a confirmation round.
        let have_form = if self.have_forms.contains(&HaveForm::Exact)
            && other.have_forms.contains(&HaveForm::Exact)
        {
            HaveForm::Exact
        } else if self.have_forms.contains(&HaveForm::Bloom)
            && other.have_forms.contains(&HaveForm::Bloom)
        {
            HaveForm::Bloom
        } else {
            return Err(ObjectError::Corrupt(
                "no have-set form in common".to_string(),
            ));
        };
        Ok(Agreement {
            protocol,
            object_format,
            have_form,
            filters: self.filters && other.filters,
            resumable: self.resumable && other.resumable,
            max_roots: self.max_roots.min(other.max_roots),
            max_objects_per_response: self
                .max_objects_per_response
                .min(other.max_objects_per_response),
        })
    }
}

fn highest_common(a: &[u16], b: &[u16]) -> Option<u16> {
    a.iter().filter(|v| b.contains(v)).max().copied()
}

/// What two sides settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreement {
    pub protocol: u16,
    pub object_format: u16,
    pub have_form: HaveForm,
    pub filters: bool,
    pub resumable: bool,
    pub max_roots: usize,
    pub max_objects_per_response: usize,
}

/// How a receiver describes what it already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaveForm {
    /// Every id, listed. Exact, prunes, and costs 33 bytes each.
    Exact,
    /// A Bloom filter. Small and constant-ish, at the price of a
    /// confirmation round for its false positives.
    Bloom,
}

/// A Bloom filter over object ids.
///
/// The hashes are taken from the digest itself rather than computed over it:
/// an object id is already a uniformly distributed 256-bit value, so slicing
/// it gives independent-enough hash functions for free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BloomHaveSet {
    bits: Vec<u8>,
    hashes: u8,
    inserted: u64,
}

impl BloomHaveSet {
    /// Size a filter for `expected` members at roughly `false_positive_rate`.
    pub fn sized_for(expected: usize, false_positive_rate: f64) -> BloomHaveSet {
        let expected = expected.max(1);
        let rate = false_positive_rate.clamp(1e-6, 0.5);
        // m = -n ln p / (ln 2)^2, k = (m/n) ln 2 — the standard sizing.
        let bits = (-(expected as f64) * rate.ln() / (std::f64::consts::LN_2.powi(2))).ceil();
        let bits = (bits as usize).clamp(64, 1 << 31);
        let hashes = ((bits as f64 / expected as f64) * std::f64::consts::LN_2)
            .round()
            .clamp(1.0, 16.0) as u8;
        BloomHaveSet {
            bits: vec![0u8; bits.div_ceil(8)],
            hashes,
            inserted: 0,
        }
    }

    pub fn insert(&mut self, id: &ObjectId) {
        let indices: Vec<usize> = self.indices(id).collect();
        for index in indices {
            self.bits[index / 8] |= 1 << (index % 8);
        }
        self.inserted += 1;
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.indices(id)
            .all(|index| self.bits[index / 8] & (1 << (index % 8)) != 0)
    }

    pub fn inserted(&self) -> u64 {
        self.inserted
    }

    pub fn byte_len(&self) -> usize {
        self.bits.len()
    }

    /// The chance a member it does not hold reads as present, given what has
    /// actually been put in.
    pub fn false_positive_rate(&self) -> f64 {
        let m = (self.bits.len() * 8) as f64;
        let k = self.hashes as f64;
        let n = self.inserted as f64;
        (1.0 - (-k * n / m).exp()).powf(k)
    }

    fn indices(&self, id: &ObjectId) -> impl Iterator<Item = usize> + '_ {
        let bits = (self.bits.len() * 8) as u64;
        let digest = id.digest;
        (0..self.hashes).map(move |round| {
            // Two 64-bit words from the digest, combined the Kirsch-Mitzenmacher
            // way: g_i(x) = h1 + i*h2. Cheap, and as good as independent
            // hashing for a filter.
            let h1 = u64::from_le_bytes(digest[0..8].try_into().expect("8 bytes"));
            let h2 = u64::from_le_bytes(digest[8..16].try_into().expect("8 bytes")) | 1;
            (h1.wrapping_add((round as u64).wrapping_mul(h2)) % bits) as usize
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bits.len() + 16);
        out.push(self.hashes);
        write_varuint(self.inserted, &mut out);
        write_varuint(self.bits.len() as u64, &mut out);
        out.extend_from_slice(&self.bits);
        out
    }

    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<BloomHaveSet> {
        let mut input = bytes;
        let Some((&hashes, rest)) = input.split_first() else {
            return Err(ObjectError::Truncated("bloom filter"));
        };
        input = rest;
        if hashes == 0 || hashes > 16 {
            return Err(ObjectError::Corrupt(format!(
                "a bloom filter with {hashes} hash rounds is not one this build made"
            )));
        }
        let inserted = read_varuint(&mut input)?;
        let len = read_varuint(&mut input)? as usize;
        if len > max_bytes {
            return Err(ObjectError::TooLarge {
                what: "bloom filter",
                len,
                max: max_bytes,
            });
        }
        if len != input.len() {
            return Err(ObjectError::Corrupt(
                "a bloom filter's declared length is not its actual length".into(),
            ));
        }
        Ok(BloomHaveSet {
            bits: input.to_vec(),
            hashes,
            inserted,
        })
    }
}

impl HaveSet for BloomHaveSet {
    fn may_have(&self, id: &ObjectId) -> bool {
        self.contains(id)
    }

    /// The point of the whole exercise: a hit is a maybe, so it must not
    /// prune. See the module docs.
    fn is_definite(&self) -> bool {
        false
    }
}

/// Build a filter over a set of ids, sized for it.
pub fn bloom_of(ids: &BTreeSet<ObjectId>, false_positive_rate: f64) -> BloomHaveSet {
    let mut filter = BloomHaveSet::sized_for(ids.len(), false_positive_rate);
    for id in ids {
        filter.insert(id);
    }
    filter
}

/// What a receiver sends to say what it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaveSummary {
    /// Nothing: a fresh clone.
    Empty,
    /// Every id, exactly.
    Exact(BTreeSet<ObjectId>),
    /// A filter over them.
    Bloom(BloomHaveSet),
}

impl HaveSummary {
    /// Choose a form for a set of this size.
    ///
    /// A small set travels exactly, because 33 bytes each is nothing and an
    /// exact set prunes whole subtrees. A large one travels as a filter, where
    /// the confirmation round costs less than the list would have.
    pub fn of(ids: BTreeSet<ObjectId>, exact_below: usize) -> HaveSummary {
        if ids.is_empty() {
            HaveSummary::Empty
        } else if ids.len() < exact_below {
            HaveSummary::Exact(ids)
        } else {
            HaveSummary::Bloom(bloom_of(&ids, 0.01))
        }
    }

    pub fn as_have_set(&self) -> &dyn HaveSet {
        match self {
            HaveSummary::Empty => &EMPTY,
            HaveSummary::Exact(set) => set,
            HaveSummary::Bloom(filter) => filter,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            HaveSummary::Empty => 0,
            HaveSummary::Exact(set) => set.len(),
            HaveSummary::Bloom(filter) => filter.inserted() as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn needs_confirmation(&self) -> bool {
        matches!(self, HaveSummary::Bloom(_))
    }
}

static EMPTY: crate::walk::HaveNothing = crate::walk::HaveNothing;

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> ObjectId {
        ObjectId::from_blake3(cavs_hash::hash_chunk(&n.to_le_bytes()))
    }

    #[test]
    fn two_default_sides_agree() {
        let agreement = Capabilities::default()
            .agree(&Capabilities::default())
            .unwrap();
        assert_eq!(agreement.protocol, PROTOCOL_V2);
        assert_eq!(agreement.have_form, HaveForm::Exact);
        assert!(agreement.resumable);
    }

    /// Negotiation is what lets one side be older than the other without
    /// either having to guess.
    #[test]
    fn the_highest_version_both_speak_wins() {
        let new = Capabilities {
            protocol_versions: vec![2, 3, 4],
            ..Default::default()
        };
        let old = Capabilities {
            protocol_versions: vec![1, 2, 3],
            ..Default::default()
        };
        assert_eq!(new.agree(&old).unwrap().protocol, 3);
    }

    #[test]
    fn sides_with_nothing_in_common_say_so() {
        let one = Capabilities {
            protocol_versions: vec![9],
            ..Default::default()
        };
        let err = one.agree(&Capabilities::default()).unwrap_err();
        assert!(err.to_string().contains("no protocol version in common"));
    }

    #[test]
    fn limits_come_down_to_the_stricter_side() {
        let strict = Capabilities {
            max_roots: 8,
            max_objects_per_response: 100,
            resumable: false,
            ..Default::default()
        };
        let agreement = Capabilities::default().agree(&strict).unwrap();
        assert_eq!(agreement.max_roots, 8);
        assert_eq!(agreement.max_objects_per_response, 100);
        assert!(!agreement.resumable);
    }

    #[test]
    fn a_filter_never_denies_something_it_holds() {
        let mut filter = BloomHaveSet::sized_for(10_000, 0.01);
        let members: Vec<ObjectId> = (0..10_000).map(id).collect();
        for member in &members {
            filter.insert(member);
        }
        for member in &members {
            assert!(filter.contains(member), "a member read as absent");
        }
        assert_eq!(filter.inserted(), 10_000);
    }

    /// The direction that costs money: a false positive means the sender
    /// leaves something out. It has to be rare, and it has to be measured
    /// rather than assumed.
    #[test]
    fn false_positives_stay_near_the_rate_asked_for() {
        let mut filter = BloomHaveSet::sized_for(10_000, 0.01);
        for member in 0..10_000 {
            filter.insert(&id(member));
        }
        let mut hits = 0;
        let trials = 20_000;
        for outsider in 1_000_000..1_000_000 + trials {
            if filter.contains(&id(outsider)) {
                hits += 1;
            }
        }
        let observed = hits as f64 / trials as f64;
        assert!(
            observed < 0.03,
            "asked for 1% false positives and measured {:.2}%",
            observed * 100.0
        );
        assert!(filter.false_positive_rate() < 0.02);
    }

    /// The reason to send a filter at all.
    #[test]
    fn a_filter_is_far_smaller_than_the_list_it_stands_for() {
        let ids: BTreeSet<ObjectId> = (0..100_000).map(id).collect();
        let filter = bloom_of(&ids, 0.01);
        let exact_bytes = ids.len() * 33;
        assert!(
            filter.byte_len() * 20 < exact_bytes,
            "filter {} bytes against {exact_bytes} for the list",
            filter.byte_len()
        );
    }

    #[test]
    fn a_filter_survives_the_wire() {
        let ids: BTreeSet<ObjectId> = (0..1_000).map(id).collect();
        let filter = bloom_of(&ids, 0.01);
        let encoded = filter.encode();
        let back = BloomHaveSet::decode(&encoded, 1 << 20).unwrap();
        assert_eq!(back, filter);
        for member in &ids {
            assert!(back.contains(member));
        }
    }

    #[test]
    fn a_damaged_filter_is_refused() {
        let filter = bloom_of(&(0..100).map(id).collect(), 0.01);
        let encoded = filter.encode();
        assert!(BloomHaveSet::decode(&encoded[..encoded.len() - 1], 1 << 20).is_err());
        assert!(BloomHaveSet::decode(&encoded, 8).is_err());
        assert!(BloomHaveSet::decode(&[], 1 << 20).is_err());
        // Zero hash rounds would make every lookup a hit.
        let mut zeroed = encoded.clone();
        zeroed[0] = 0;
        assert!(BloomHaveSet::decode(&zeroed, 1 << 20).is_err());
    }

    #[test]
    fn a_summary_picks_its_form_by_size() {
        let small: BTreeSet<ObjectId> = (0..10).map(id).collect();
        let large: BTreeSet<ObjectId> = (0..10_000).map(id).collect();
        assert!(matches!(
            HaveSummary::of(small, 1_000),
            HaveSummary::Exact(_)
        ));
        assert!(matches!(
            HaveSummary::of(large, 1_000),
            HaveSummary::Bloom(_)
        ));
        assert!(matches!(
            HaveSummary::of(BTreeSet::new(), 1_000),
            HaveSummary::Empty
        ));
    }

    /// An exact summary prunes; a filter does not, whatever else changes.
    #[test]
    fn only_an_exact_summary_is_definite() {
        let ids: BTreeSet<ObjectId> = (0..10).map(id).collect();
        assert!(HaveSummary::Exact(ids.clone()).as_have_set().is_definite());
        assert!(!HaveSummary::Bloom(bloom_of(&ids, 0.01))
            .as_have_set()
            .is_definite());
        assert!(HaveSummary::Empty.as_have_set().is_definite());
        assert!(HaveSummary::Bloom(bloom_of(&ids, 0.01)).needs_confirmation());
    }
}
