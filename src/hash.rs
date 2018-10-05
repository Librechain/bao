use arrayvec::ArrayVec;
use blake2b_simd;
use byteorder::{ByteOrder, LittleEndian};
use core::cmp;
use core::fmt;
use core::mem;
#[cfg(feature = "std")]
use rayon;
#[cfg(feature = "std")]
use std::io;

pub const HASH_SIZE: usize = 32;
pub(crate) const PARENT_SIZE: usize = 2 * HASH_SIZE;
pub(crate) const HEADER_SIZE: usize = 8;
pub(crate) const CHUNK_SIZE: usize = 4096;
// NOTE: MAX_DEPTH should be 52, given the 4096 byte CHUNK_SIZE, using a larger value wastes some
// space on the stack. It currently needs to match one of the implementations of arrayvec::Array,
// but dropping that dependency could let us compute MAX_DEPTH from other parameters.
pub(crate) const MAX_DEPTH: usize = 64;
pub(crate) const MAX_SINGLE_THREADED: usize = 4 * CHUNK_SIZE;

pub type Hash = [u8; HASH_SIZE];
pub(crate) type ParentNode = [u8; 2 * HASH_SIZE];

pub(crate) fn encode_len(len: u64) -> [u8; HEADER_SIZE] {
    debug_assert_eq!(mem::size_of_val(&len), HEADER_SIZE);
    let mut len_bytes = [0; HEADER_SIZE];
    LittleEndian::write_u64(&mut len_bytes, len);
    len_bytes
}

pub(crate) fn decode_len(bytes: &[u8; HEADER_SIZE]) -> u64 {
    LittleEndian::read_u64(bytes)
}

pub(crate) fn new_blake2b_state() -> blake2b_simd::State {
    blake2b_simd::Params::new()
        .hash_length(HASH_SIZE)
        .to_state()
}

// The root node is hashed differently from interior nodes. It gets suffixed
// with the length of the entire input, and we set the Blake2 final node flag.
// That means that no root hash can ever collide with an interior hash, or with
// the root of a different size tree.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Finalization {
    NotRoot,
    Root(u64),
}
use self::Finalization::{NotRoot, Root};

pub(crate) fn finalize_hash(state: &mut blake2b_simd::State, finalization: Finalization) -> Hash {
    // For the root node, we hash in the length as a suffix, and we set the
    // Blake2 last node flag. One of the reasons for this design is that we
    // don't need to know a given node is the root until the very end, so we
    // don't always need a chunk buffer.
    if let Root(root_len) = finalization {
        state.update(&encode_len(root_len));
        state.set_last_node(true);
    }
    let blake_digest = state.finalize();
    *array_ref!(blake_digest.as_bytes(), 0, HASH_SIZE)
}

pub(crate) fn hash_node(chunk: &[u8], finalization: Finalization) -> Hash {
    debug_assert!(chunk.len() <= CHUNK_SIZE);
    let mut state = new_blake2b_state();
    state.update(chunk);
    finalize_hash(&mut state, finalization)
}

pub(crate) fn parent_hash(left_hash: &Hash, right_hash: &Hash, finalization: Finalization) -> Hash {
    let mut state = new_blake2b_state();
    state.update(left_hash);
    state.update(right_hash);
    finalize_hash(&mut state, finalization)
}

// Find the largest power of two that's less than or equal to `n`. We use this
// for computing subtree sizes below.
pub(crate) fn largest_power_of_two_leq(n: u64) -> u64 {
    ((n / 2) + 1).next_power_of_two()
}

// Given some input larger than one chunk, find the largest perfect tree of
// chunks that can go on the left.
pub(crate) fn left_len(content_len: u64) -> u64 {
    debug_assert!(content_len > CHUNK_SIZE as u64);
    // Subtract 1 to reserve at least one byte for the right side.
    let full_chunks = (content_len - 1) / CHUNK_SIZE as u64;
    largest_power_of_two_leq(full_chunks) * CHUNK_SIZE as u64
}

fn hash_recurse(input: &[u8], finalization: Finalization) -> Hash {
    if input.len() <= CHUNK_SIZE {
        return hash_node(input, finalization);
    }
    // If we have more than one chunk of input, recursively hash the left and
    // right sides. The left_len() function determines the shape of the tree.
    let (left, right) = input.split_at(left_len(input.len() as u64) as usize);
    // Child nodes are never the root.
    let left_hash = hash_recurse(left, NotRoot);
    let right_hash = hash_recurse(right, NotRoot);
    parent_hash(&left_hash, &right_hash, finalization)
}

#[cfg(feature = "std")]
fn hash_recurse_rayon(input: &[u8], finalization: Finalization) -> Hash {
    if input.len() <= CHUNK_SIZE {
        return hash_node(input, finalization);
    }
    let (left, right) = input.split_at(left_len(input.len() as u64) as usize);
    let (left_hash, right_hash) = rayon::join(
        || hash_recurse_rayon(left, NotRoot),
        || hash_recurse_rayon(right, NotRoot),
    );
    parent_hash(&left_hash, &right_hash, finalization)
}

/// Hash a slice of input bytes all at once. Above about 16 kilobytes, this will parallelize using
/// [Rayon](https://crates.io/crates/rayon).
pub fn hash(input: &[u8]) -> Hash {
    #[cfg(feature = "std")]
    {
        // Below about 4 chunks, the overhead of parallelizing isn't worth it.
        if input.len() <= MAX_SINGLE_THREADED {
            hash_recurse(input, Root(input.len() as u64))
        } else {
            hash_recurse_rayon(input, Root(input.len() as u64))
        }
    }
    #[cfg(not(feature = "std"))]
    {
        hash_recurse(input, Root(input.len() as u64))
    }
}

pub(crate) enum StateFinish {
    Parent(ParentNode),
    Root(Hash),
}

/// A minimal state object for incrementally hashing input. Most callers should use the `Writer`
/// interface instead.
///
/// This is designed to be useful for as many callers as possible, including `no_std` callers. It
/// handles merging subtrees and keeps track of subtrees assembled so far. It takes only hashes as
/// input, rather than raw input bytes, so it can be used with e.g. multiple threads hashing chunks
/// in parallel. Callers that need `ParentNode` bytes for building the encoded tree, can use the
/// optional `merge_parent` and `merge_finish` interfaces.
///
/// This struct contains a relatively large buffer on the stack for holding partial subtree hashes:
/// 64 hashes at 32 bytes apiece, 2048 bytes in total. This is enough state space for the largest
/// possible input, `2^64 - 1` bytes or about 18 exabytes. That's impractically large for anything
/// that could be hashed in the real world, and implementations that are starved for stack space
/// could cut that buffer in half and still be able to hash about 17 terabytes (`2^32` times the
/// 4096-byte chunk size).
#[derive(Clone)]
pub(crate) struct State {
    subtrees: ArrayVec<[Hash; MAX_DEPTH]>,
    total_len: u64,
}

impl State {
    pub fn new() -> Self {
        Self {
            subtrees: ArrayVec::new(),
            total_len: 0,
        }
    }

    fn count(&self) -> u64 {
        self.total_len
    }

    fn merge_inner(&mut self, finalization: Finalization) -> ParentNode {
        let right_child = self.subtrees.pop().unwrap();
        let left_child = self.subtrees.pop().unwrap();
        let mut parent_node = [0; PARENT_SIZE];
        parent_node[..HASH_SIZE].copy_from_slice(&left_child);
        parent_node[HASH_SIZE..].copy_from_slice(&right_child);
        let parent_hash = parent_hash(&left_child, &right_child, finalization);
        self.subtrees.push(parent_hash);
        parent_node
    }

    // We keep the subtree hashes in an array without storing their size, and we use this cute
    // trick to figure out when we should merge them. Because every subtree (prior to the
    // finalization step) is a power of two times the chunk size, adding a new subtree to the
    // right/small end is a lot like adding a 1 to a binary number, and merging subtrees is like
    // propagating the carry bit. Each carry represents a place where two subtrees need to be
    // merged, and the final number of 1 bits is the same as the final number of subtrees.
    fn needs_merge(&self) -> bool {
        let chunks = self.total_len / CHUNK_SIZE as u64;
        self.subtrees.len() > chunks.count_ones() as usize
    }

    /// Add a subtree hash to the state.
    ///
    /// For most callers, this will always be the hash of a `CHUNK_SIZE` chunk of input bytes, with
    /// the final chunk possibly having fewer bytes. It's possible to use input subtrees larger
    /// than a single chunk, as long as the size is a power of 2 times `CHUNK_SIZE` and again kept
    /// constant until the final chunk. This can be helpful in a multi-threaded setting, where you
    /// want to hash more than one chunk at a time per thread, but most callers should stick with
    /// single chunks.
    ///
    /// In cases where the total input is a single chunk or less, including the case with no input
    /// bytes at all, callers are expected to finalize that chunk themselves before pushing. (Or
    /// just ignore the State object entirely.) It's of course impossible to back out the input
    /// bytes and re-finalize them.
    pub fn push_subtree(&mut self, hash: &Hash, len: usize) {
        // Merge any subtrees that need to be merged before pushing. In the encoding case, the
        // caller will already have done this via merge_parent(), but in the hashing case the
        // caller doesn't care about the parent nodes.
        while self.needs_merge() {
            self.merge_inner(NotRoot);
        }
        self.subtrees.push(*hash);
        self.total_len += len as u64;
    }

    /// Returns a `ParentNode` corresponding to a just-completed subtree, if any.
    ///
    /// Callers that want parent node bytes (to build an encoded tree) must call `merge_parent` in
    /// a loop, until it returns `None`. Parent nodes are yielded in smallest-to-largest order.
    /// Callers that only want the final root hash can ignore this function; the next call to
    /// `push_subtree` will take care of merging in that case.
    ///
    /// After the final call to `push_subtree`, you must call `merge_finish` in a loop instead of
    /// this function.
    pub fn merge_parent(&mut self) -> Option<ParentNode> {
        if !self.needs_merge() {
            return None;
        }
        Some(self.merge_inner(NotRoot))
    }

    /// Returns a tuple of `ParentNode` bytes and (in the last call only) the root hash. Callers
    /// who need `ParentNode` bytes must call `merge_finish` in a loop after pushing the final
    /// subtree, until the second return value is `Some`. Callers who don't need parent nodes
    /// should use the simpler `finish` interface instead.
    pub fn merge_finish(&mut self) -> StateFinish {
        if self.subtrees.len() > 2 {
            StateFinish::Parent(self.merge_inner(NotRoot))
        } else if self.subtrees.len() == 2 {
            let root_finalization = Root(self.total_len); // Appease borrowck.
            StateFinish::Parent(self.merge_inner(root_finalization))
        } else {
            StateFinish::Root(self.subtrees[0])
        }
    }

    /// A wrapper around `merge_finish` for callers who don't need the parent
    /// nodes.
    pub fn finish(&mut self) -> Hash {
        loop {
            match self.merge_finish() {
                StateFinish::Parent(_) => {} // ignored
                StateFinish::Root(root) => return root,
            }
        }
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Avoid printing hashes, they might be secret.
        write!(f, "State {{ ... }}")
    }
}

#[derive(Clone, Debug)]
pub struct Writer {
    chunk: blake2b_simd::State,
    state: State,
}

impl Writer {
    pub fn new() -> Self {
        Self {
            chunk: new_blake2b_state(),
            state: State::new(),
        }
    }

    /// This is equivalent to `write`, except that it's also available with `no_std`.
    pub fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            if self.chunk.count() as usize == CHUNK_SIZE {
                let hash = finalize_hash(&mut self.chunk, NotRoot);
                self.state.push_subtree(&hash, CHUNK_SIZE);
                self.chunk = new_blake2b_state();
            }
            let want = CHUNK_SIZE - self.chunk.count() as usize;
            let take = cmp::min(want, input.len());
            self.chunk.update(&input[..take]);
            input = &input[take..];
        }
    }

    /// After feeding all the input bytes to `write`, return the root hash. The writer cannot be
    /// used after this.
    pub fn finish(&mut self) -> Hash {
        let finalization = if self.state.count() == 0 {
            Root(self.chunk.count() as u64)
        } else {
            NotRoot
        };
        let hash = finalize_hash(&mut self.chunk, finalization);
        self.state.push_subtree(&hash, self.chunk.count() as usize);
        self.state.finish()
    }
}

#[cfg(feature = "std")]
impl io::Write for Writer {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.update(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[doc(hidden)]
pub mod benchmarks {
    pub const HEADER_SIZE: usize = super::HEADER_SIZE;
    pub const CHUNK_SIZE: usize = super::CHUNK_SIZE;
}

// Interesting input lengths to run tests on.
#[cfg(test)]
pub(crate) const TEST_CASES: &[usize] = &[
    0,
    1,
    10,
    CHUNK_SIZE - 1,
    CHUNK_SIZE,
    CHUNK_SIZE + 1,
    2 * CHUNK_SIZE - 1,
    2 * CHUNK_SIZE,
    2 * CHUNK_SIZE + 1,
    3 * CHUNK_SIZE - 1,
    3 * CHUNK_SIZE,
    3 * CHUNK_SIZE + 1,
    4 * CHUNK_SIZE - 1,
    4 * CHUNK_SIZE,
    4 * CHUNK_SIZE + 1,
    16 * CHUNK_SIZE - 1,
    16 * CHUNK_SIZE,
    16 * CHUNK_SIZE + 1,
];

#[cfg(test)]
mod test {
    use super::*;
    use std::io::prelude::*;

    #[test]
    fn test_power_of_two() {
        let input_output = &[
            // The zero case is nonsensical, but it does work.
            (0, 1),
            (1, 1),
            (2, 2),
            (3, 2),
            (4, 4),
            (5, 4),
            (6, 4),
            (7, 4),
            (8, 8),
            // the largest possible u64
            (0xffffffffffffffff, 0x8000000000000000),
        ];
        for &(input, output) in input_output {
            assert_eq!(
                output,
                largest_power_of_two_leq(input),
                "wrong output for n={}",
                input
            );
        }
    }

    #[test]
    fn test_left_subtree_len() {
        let s = CHUNK_SIZE as u64;
        let input_output = &[(s + 1, s), (2 * s - 1, s), (2 * s, s), (2 * s + 1, 2 * s)];
        for &(input, output) in input_output {
            println!("testing {} and {}", input, output);
            assert_eq!(left_len(input), output);
        }
    }

    #[test]
    fn test_serial_vs_parallel() {
        for &case in TEST_CASES {
            println!("case {}", case);
            let input = vec![0x42; case];
            let hash_serial = hash_recurse(&input, Root(case as u64));
            let hash_parallel = hash_recurse_rayon(&input, Root(case as u64));
            let hash_highlevel = hash(&input);
            assert_eq!(hash_serial, hash_parallel, "hashes don't match");
            assert_eq!(hash_serial, hash_highlevel, "hashes don't match");
        }
    }

    fn drive_state(mut input: &[u8]) -> Hash {
        let mut state = State::new();
        let finalization = if input.len() <= CHUNK_SIZE {
            Root(input.len() as u64)
        } else {
            NotRoot
        };
        while input.len() > CHUNK_SIZE {
            let hash = hash_node(&input[..CHUNK_SIZE], NotRoot);
            state.push_subtree(&hash, CHUNK_SIZE);
            input = &input[CHUNK_SIZE..];
        }
        let hash = hash_node(input, finalization);
        state.push_subtree(&hash, input.len());
        state.finish()
    }

    #[test]
    fn test_state() {
        for &case in TEST_CASES {
            println!("case {}", case);
            let input = vec![0x42; case];
            let expected = hash(&input);
            let found = drive_state(&input);
            assert_eq!(expected, found, "hashes don't match");
        }
    }

    #[test]
    fn test_writer() {
        for &case in TEST_CASES {
            println!("case {}", case);
            let input = vec![0x42; case];
            let expected = hash(&input);

            let mut writer = Writer::new();
            writer.write_all(&input).unwrap();
            let found = writer.finish();
            assert_eq!(expected, found, "hashes don't match");
        }
    }
}
