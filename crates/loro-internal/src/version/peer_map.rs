//! A persistent hash map keyed by [`PeerID`], the storage behind
//! [`ImVersionVector`](super::ImVersionVector).
//!
//! Cloning is O(1): clones share every node, and a write copies only the
//! nodes on the path to the entry it changes (copy on write). Lookups,
//! inserts and removals visit O(log₃₂ n) nodes.
//!
//! The map is a hash array mapped trie (HAMT). Each node has 32 slots indexed
//! by five bits of the key's hash and stores only its occupied slots, so a
//! small map is a single node holding exactly its entries. Keys are spread by
//! a bijective 64-bit mixer, so distinct peers always have distinct hashes:
//! two keys always part ways within [`MAX_DEPTH`] levels and the trie needs
//! no collision buckets.

use std::{fmt, iter::FusedIterator, marker::PhantomData, sync::Arc};

use serde::{
    de::{MapAccess, Visitor},
    ser::SerializeMap,
    Deserialize, Deserializer, Serialize, Serializer,
};
use smallvec::SmallVec;

use crate::PeerID;

const BITS: u32 = 5;
const MASK: u64 = (1 << BITS) - 1;
/// The number of trie levels a 64-bit hash covers; the last level uses the
/// four bits that remain.
const MAX_DEPTH: u32 = u64::BITS.div_ceil(BITS);

/// Fibonacci hashing: a multiplication by an odd constant is a bijection on
/// `u64`, so distinct peers never collide. Its high bits are the well-mixed
/// ones, so [`slot`] reads the hash from the top down.
#[inline]
fn mix(peer: PeerID) -> u64 {
    peer.wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// The five hash bits that pick the slot at `depth`, from the top bits down.
/// The last level takes the four lowest bits and the top bit again, which
/// still separates any two distinct hashes.
#[inline]
fn slot(hash: u64, depth: u32) -> u32 {
    debug_assert!(depth < MAX_DEPTH);
    (hash.rotate_left((depth + 1) * BITS) & MASK) as u32
}

#[inline]
fn has(bitmap: u32, bit: u32) -> bool {
    bitmap & (1 << bit) != 0
}

/// The position of slot `bit` among a node's occupied slots.
#[inline]
fn index(bitmap: u32, bit: u32) -> usize {
    (bitmap & ((1 << bit) - 1)).count_ones() as usize
}

/// A trie node is a bitmap of its occupied slots and the entries of those
/// slots, in slot order. The bitmap lives beside the pointer to the entries,
/// so each level costs one pointer hop.
type Entries<V> = Arc<[Entry<V>]>;

#[derive(Clone)]
enum Entry<V> {
    Leaf(PeerID, V),
    Node(u32, Entries<V>),
}

/// The entries of a node, copied first if another map shares them.
fn make_mut<V: Clone>(entries: &mut Entries<V>) -> &mut [Entry<V>] {
    if Arc::get_mut(entries).is_none() {
        *entries = Arc::from(&entries[..]);
    }

    Arc::get_mut(entries).unwrap()
}

/// A node holding two leaves whose keys differ, at `depth`.
fn pair<V>(a: (PeerID, V), b: (PeerID, V), b_hash: u64, depth: u32) -> (u32, Entries<V>) {
    let a_bit = slot(mix(a.0), depth);
    let b_bit = slot(b_hash, depth);
    if a_bit == b_bit {
        let (bitmap, entries) = pair(a, b, b_hash, depth + 1);
        return (1 << a_bit, Arc::from([Entry::Node(bitmap, entries)]));
    }

    let (a, b) = (Entry::Leaf(a.0, a.1), Entry::Leaf(b.0, b.1));
    let entries = if a_bit < b_bit { [a, b] } else { [b, a] };
    ((1 << a_bit) | (1 << b_bit), Arc::from(entries))
}

fn insert<V: Clone>(
    bitmap: &mut u32,
    entries: &mut Entries<V>,
    peer: PeerID,
    hash: u64,
    value: V,
    depth: u32,
) -> Option<V> {
    let bit = slot(hash, depth);
    let at = index(*bitmap, bit);
    if !has(*bitmap, bit) {
        *bitmap |= 1 << bit;
        *entries = entries[..at]
            .iter()
            .cloned()
            .chain(std::iter::once(Entry::Leaf(peer, value)))
            .chain(entries[at..].iter().cloned())
            .collect();
        return None;
    }

    let slots = make_mut(entries);
    match &mut slots[at] {
        Entry::Node(child_bitmap, child) => {
            insert(child_bitmap, child, peer, hash, value, depth + 1)
        }
        Entry::Leaf(key, old) if *key == peer => Some(std::mem::replace(old, value)),
        Entry::Leaf(key, old) => {
            let (child_bitmap, child) = pair((*key, old.clone()), (peer, value), hash, depth + 1);
            slots[at] = Entry::Node(child_bitmap, child);
            None
        }
    }
}

/// Removes `peer`, which must be present.
fn remove<V: Clone>(
    bitmap: &mut u32,
    entries: &mut Entries<V>,
    peer: PeerID,
    hash: u64,
    depth: u32,
) -> V {
    let bit = slot(hash, depth);
    let at = index(*bitmap, bit);
    if let Entry::Leaf(key, value) = &entries[at] {
        debug_assert_eq!(*key, peer);
        let value = value.clone();
        *bitmap &= !(1 << bit);
        *entries = entries[..at]
            .iter()
            .chain(&entries[at + 1..])
            .cloned()
            .collect();
        return value;
    }

    let slots = make_mut(entries);
    let Entry::Node(child_bitmap, child) = &mut slots[at] else {
        unreachable!()
    };
    let value = remove(child_bitmap, child, peer, hash, depth + 1);
    // A child left with a single leaf folds back into this node, so the trie
    // stays only as deep as its keys need.
    let fold = match &child[..] {
        [leaf @ Entry::Leaf(..)] => Some(leaf.clone()),
        _ => None,
    };
    if let Some(leaf) = fold {
        slots[at] = leaf;
    }

    value
}

/// A persistent map from [`PeerID`] to `V` with O(1) clones and
/// structurally shared copy-on-write updates. See the module docs.
pub struct PeerMap<V> {
    bitmap: u32,
    root: Option<Entries<V>>,
    len: usize,
}

impl<V> Default for PeerMap<V> {
    fn default() -> Self {
        Self {
            bitmap: 0,
            root: None,
            len: 0,
        }
    }
}

impl<V> Clone for PeerMap<V> {
    fn clone(&self) -> Self {
        Self {
            bitmap: self.bitmap,
            root: self.root.clone(),
            len: self.len,
        }
    }
}

impl<V> PeerMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn get(&self, peer: &PeerID) -> Option<&V> {
        let hash = mix(*peer);
        let mut bitmap = self.bitmap;
        let mut entries = self.root.as_deref()?;
        let mut depth = 0;
        loop {
            let bit = slot(hash, depth);
            if !has(bitmap, bit) {
                return None;
            }

            match &entries[index(bitmap, bit)] {
                Entry::Leaf(key, value) => return (key == peer).then_some(value),
                Entry::Node(child_bitmap, child) => {
                    bitmap = *child_bitmap;
                    entries = child;
                    depth += 1;
                }
            }
        }
    }

    pub fn contains_key(&self, peer: &PeerID) -> bool {
        self.get(peer).is_some()
    }

    /// Iterates over the entries in an unspecified order.
    pub fn iter(&self) -> Iter<'_, V> {
        let mut stack = SmallVec::new();
        if let Some(root) = &self.root {
            stack.push(root.iter());
        }

        Iter {
            stack,
            remaining: self.len,
        }
    }
}

impl<V: Clone> PeerMap<V> {
    /// Returns a mutable reference to the value of `peer`. Nodes shared with
    /// other clones are copied on the path to it; a missing key copies nothing.
    pub fn get_mut(&mut self, peer: &PeerID) -> Option<&mut V> {
        if !self.contains_key(peer) {
            return None;
        }

        let hash = mix(*peer);
        let mut bitmap = self.bitmap;
        let mut entries = make_mut(self.root.as_mut()?);
        let mut depth = 0;
        loop {
            match &mut entries[index(bitmap, slot(hash, depth))] {
                Entry::Leaf(_, value) => return Some(value),
                Entry::Node(child_bitmap, child) => {
                    bitmap = *child_bitmap;
                    entries = make_mut(child);
                    depth += 1;
                }
            }
        }
    }

    /// Inserts `value` for `peer` and returns the value it replaced.
    pub fn insert(&mut self, peer: PeerID, value: V) -> Option<V> {
        let hash = mix(peer);
        let Some(root) = &mut self.root else {
            self.bitmap = 1 << slot(hash, 0);
            self.root = Some(Arc::from([Entry::Leaf(peer, value)]));
            self.len = 1;
            return None;
        };

        let old = insert(&mut self.bitmap, root, peer, hash, value, 0);
        if old.is_none() {
            self.len += 1;
        }

        old
    }

    /// Removes `peer` and returns its value. A missing key copies nothing.
    pub fn remove(&mut self, peer: &PeerID) -> Option<V> {
        if !self.contains_key(peer) {
            return None;
        }

        let value = remove(&mut self.bitmap, self.root.as_mut()?, *peer, mix(*peer), 0);
        self.len -= 1;
        if self.len == 0 {
            self.root = None;
        }

        Some(value)
    }
}

impl<V: Clone> FromIterator<(PeerID, V)> for PeerMap<V> {
    fn from_iter<I: IntoIterator<Item = (PeerID, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (peer, value) in iter {
            map.insert(peer, value);
        }

        map
    }
}

impl<V: fmt::Debug> fmt::Debug for PeerMap<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// An iterator over the entries of a [`PeerMap`], in an unspecified order.
pub struct Iter<'a, V> {
    /// The entries left in each node on the path from the root.
    stack: SmallVec<[std::slice::Iter<'a, Entry<V>>; 4]>,
    remaining: usize,
}

impl<V> Clone for Iter<'_, V> {
    fn clone(&self) -> Self {
        Self {
            stack: self.stack.clone(),
            remaining: self.remaining,
        }
    }
}

impl<V> fmt::Debug for Iter<'_, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Iter")
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

impl<'a, V> Iterator for Iter<'a, V> {
    type Item = (&'a PeerID, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stack.last_mut()?.next() {
                Some(Entry::Leaf(peer, value)) => {
                    self.remaining -= 1;
                    return Some((peer, value));
                }
                Some(Entry::Node(_, child)) => self.stack.push(child.iter()),
                None => {
                    self.stack.pop();
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<V> ExactSizeIterator for Iter<'_, V> {}

impl<V> FusedIterator for Iter<'_, V> {}

impl<'a, V> IntoIterator for &'a PeerMap<V> {
    type Item = (&'a PeerID, &'a V);
    type IntoIter = Iter<'a, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Serialized as a map, like the other version vectors.
impl<V: Serialize> Serialize for PeerMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.len))?;
        for (peer, value) in self.iter() {
            map.serialize_entry(peer, value)?;
        }

        map.end()
    }
}

impl<'de, V: Deserialize<'de> + Clone> Deserialize<'de> for PeerMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PeerMapVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de> + Clone> Visitor<'de> for PeerMapVisitor<V> {
            type Value = PeerMap<V>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map from peer ids")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut map = PeerMap::new();
                while let Some((peer, value)) = access.next_entry()? {
                    map.insert(peer, value);
                }

                Ok(map)
            }
        }

        deserializer.deserialize_map(PeerMapVisitor(PhantomData))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use rustc_hash::{FxHashMap, FxHashSet};

    /// Checks the trie's shape: bitmaps match the entries, every leaf sits
    /// on its hash path, and no node below the root is empty or a lone leaf.
    fn check_node<V>(bitmap: u32, entries: &[Entry<V>], path: &mut Vec<u32>) -> usize {
        assert_eq!(bitmap.count_ones() as usize, entries.len());
        if !path.is_empty() {
            assert!(!matches!(entries, [] | [Entry::Leaf(..)]));
        }

        let mut count = 0;
        let mut bits = bitmap;
        for entry in entries {
            path.push(bits.trailing_zeros());
            bits &= bits - 1;
            match entry {
                Entry::Leaf(peer, _) => {
                    for (depth, &bit) in path.iter().enumerate() {
                        assert_eq!(slot(mix(*peer), depth as u32), bit);
                    }
                    count += 1;
                }
                Entry::Node(child_bitmap, child) => count += check_node(*child_bitmap, child, path),
            }
            path.pop();
        }

        count
    }

    fn check<V: Clone + PartialEq + fmt::Debug>(map: &PeerMap<V>, model: &FxHashMap<PeerID, V>) {
        assert_eq!(map.len(), model.len());
        assert_eq!(map.is_empty(), model.is_empty());
        assert_eq!(map.iter().len(), model.len());
        assert_eq!(map.root.is_none(), model.is_empty());
        let count = map
            .root
            .as_deref()
            .map_or(0, |root| check_node(map.bitmap, root, &mut Vec::new()));
        assert_eq!(count, model.len());
        for (peer, value) in model {
            assert_eq!(map.get(peer), Some(value));
        }

        let seen: FxHashMap<PeerID, V> = map.iter().map(|(&k, v)| (k, v.clone())).collect();
        assert_eq!(&seen, model);
    }

    #[test]
    fn mix_is_injective_on_samples() {
        let mut seen = FxHashSet::default();
        for peer in (0..10_000u64).chain([u64::MAX, u64::MAX - 1, 1 << 63]) {
            assert!(seen.insert(mix(peer)));
        }
    }

    #[test]
    fn empty() {
        let map = PeerMap::<i32>::new();
        assert!(map.is_empty());
        assert_eq!(map.get(&0), None);
        assert_eq!(map.iter().next(), None);
        assert_eq!(format!("{map:?}"), "{}");
    }

    #[test]
    fn insert_get_remove_against_a_model() {
        let mut rng = StdRng::seed_from_u64(1);
        for range in [4u64, 64, 4096, u64::MAX] {
            let mut map = PeerMap::new();
            let mut model = FxHashMap::default();
            for _ in 0..5_000 {
                let peer = rng.gen_range(0..range);
                let value: i32 = rng.gen();
                match rng.gen_range(0..4) {
                    0 => assert_eq!(map.remove(&peer), model.remove(&peer)),
                    1 => {
                        if let Some(value) = map.get_mut(&peer) {
                            *value += 1;
                        }
                        if let Some(value) = model.get_mut(&peer) {
                            *value += 1;
                        }
                    }
                    _ => assert_eq!(map.insert(peer, value), model.insert(peer, value)),
                }
            }

            check(&map, &model);
            for peer in model.keys().copied().collect::<Vec<_>>() {
                assert_eq!(map.remove(&peer), model.remove(&peer));
            }
            check(&map, &model);
        }
    }

    #[test]
    fn deep_paths_split_and_fold() {
        // Two peers whose hashes share their first three slots sit four
        // levels deep; removing one folds the path back into the root.
        let path = |peer| {
            (0..3)
                .map(|depth| slot(mix(peer), depth))
                .collect::<Vec<_>>()
        };
        let twin = (1..).find(|&peer| path(peer) == path(0)).unwrap();
        let mut map = PeerMap::new();
        map.insert(0, 0);
        map.insert(twin, 1);
        let mut model: FxHashMap<PeerID, i32> = [(0, 0), (twin, 1)].into_iter().collect();
        check(&map, &model);
        assert!(matches!(map.root.as_deref().unwrap(), [Entry::Node(..)]));

        assert_eq!(map.remove(&twin), Some(1));
        model.remove(&twin);
        check(&map, &model);
        assert!(matches!(map.root.as_deref().unwrap(), [Entry::Leaf(0, 0)]));
    }

    #[test]
    fn hashes_that_differ_only_in_the_last_level() {
        // Peers are chosen through the multiplier's inverse (Newton's
        // iteration), so their hashes share all but the lowest bits and the
        // trie has to go to its last level to tell them apart.
        let k: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut inverse = k;
        for _ in 0..5 {
            inverse = inverse.wrapping_mul(2u64.wrapping_sub(k.wrapping_mul(inverse)));
        }
        let hashes = [
            0x1234_5678_9abc_def0u64,
            0x1234_5678_9abc_def1,
            0x1234_5678_9abc_def8,
        ];
        let peers: Vec<PeerID> = hashes.iter().map(|h| h.wrapping_mul(inverse)).collect();
        for (peer, hash) in peers.iter().zip(hashes) {
            assert_eq!(mix(*peer), hash);
        }

        let mut map = PeerMap::new();
        let mut model = FxHashMap::default();
        for (value, &peer) in peers.iter().enumerate() {
            assert_eq!(map.insert(peer, value), model.insert(peer, value));
            check(&map, &model);
        }
        for peer in &peers {
            assert_eq!(map.remove(peer), model.remove(peer));
            check(&map, &model);
        }
    }

    #[test]
    fn clones_are_independent() {
        let mut rng = StdRng::seed_from_u64(2);
        let mut versions: Vec<(PeerMap<i32>, FxHashMap<PeerID, i32>)> =
            vec![(PeerMap::new(), FxHashMap::default())];
        for _ in 0..2_000 {
            let (mut map, mut model) = versions[rng.gen_range(0..versions.len())].clone();
            let peer = rng.gen_range(0..300);
            match rng.gen_range(0..5) {
                0 => assert_eq!(map.remove(&peer), model.remove(&peer)),
                1 => {
                    if let Some(value) = map.get_mut(&peer) {
                        *value -= 1;
                    }
                    if let Some(value) = model.get_mut(&peer) {
                        *value -= 1;
                    }
                }
                _ => {
                    let value = rng.gen();
                    assert_eq!(map.insert(peer, value), model.insert(peer, value));
                }
            }
            versions.push((map, model));
        }

        for (map, model) in &versions {
            check(map, model);
        }
    }

    #[test]
    fn writes_copy_only_the_changed_path() {
        let base: PeerMap<i32> = (0..10_000).map(|peer| (peer, 0)).collect();
        let mut copy = base.clone();
        copy.insert(7, 1);
        let (Some(a), Some(b)) = (&base.root, &copy.root) else {
            unreachable!()
        };
        assert!(!Arc::ptr_eq(a, b));
        let shared = a
            .iter()
            .zip(b.iter())
            .filter(
                |pair| matches!(pair, (Entry::Node(_, x), Entry::Node(_, y)) if Arc::ptr_eq(x, y)),
            )
            .count();
        assert_eq!(shared, a.len() - 1);

        // Reads and writes to missing keys leave the sharing intact.
        let mut copy = base.clone();
        assert_eq!(copy.get_mut(&20_000), None);
        assert_eq!(copy.remove(&20_000), None);
        assert!(Arc::ptr_eq(
            base.root.as_ref().unwrap(),
            copy.root.as_ref().unwrap()
        ));
    }

    #[test]
    fn serde_matches_a_hash_map() {
        let map: PeerMap<i32> = [(1, 10), (u64::MAX, -1), (42, 0)].into_iter().collect();
        let model: FxHashMap<PeerID, i32> = map.iter().map(|(&k, &v)| (k, v)).collect();
        let bytes = postcard::to_allocvec(&map).unwrap();
        let decoded: FxHashMap<PeerID, i32> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, model);
        let decoded: PeerMap<i32> =
            postcard::from_bytes(&postcard::to_allocvec(&model).unwrap()).unwrap();
        check(&decoded, &model);
        let json = serde_json::to_string(&map).unwrap();
        let decoded: PeerMap<i32> = serde_json::from_str(&json).unwrap();
        check(&decoded, &model);
    }
}
