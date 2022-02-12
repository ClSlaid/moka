//! Segmented lock-free hash tables.
//!
//! Segmented hash tables divide their entries between a number of smaller
//! logical hash tables, or segments. Each segment is entirely independent from
//! the others, and entries are never relocated across segment boundaries.
//!
//! In the context of this crate, a segment refers specifically to an array of
//! bucket pointers. The number of segments in a hash table is rounded up to the
//! nearest power of two; this is so that selecting the segment for a key is no
//! more than a right shift to select the most significant bits of a hashed key.
//!
//! Each segment is entirely independent from the others, all operations can be
//! performed concurrently by multiple threads. Should a set of threads be
//! operating on disjoint sets of segments, the only synchronization between
//! them will be destructive interference as they access and update the bucket
//! array pointer and length for each segment.
//!
//! Compared to the unsegmented hash tables in this crate, the segmented hash
//! tables have higher concurrent write throughput for disjoint sets of keys.
//! However, the segmented hash tables have slightly lower read and
//! single-threaded write throughput. This is because the segmenting structure
//! adds another layer of indirection between the hash table and its buckets.
//!
//! The idea for segmenting hash tables was inspired by the
//! [`ConcurrentHashMap`] from OpenJDK 7, which consists of a number of
//! separately-locked segments. OpenJDK 8 introduced a striped concurrent hash
//! map that stripes a set of bucket locks across the set of buckets using the
//! least significant bits of hashed keys.
//!
//! [`ConcurrentHashMap`]: https://github.com/openjdk-mirror/jdk7u-jdk/blob/master/src/share/classes/java/util/concurrent/ConcurrentHashMap.java

use crate::cht::map::{
    bucket::{self, BucketArray},
    bucket_array_ref::BucketArrayRef,
    DefaultHashBuilder,
};

use std::{
    borrow::Borrow,
    hash::{BuildHasher, Hash},
    ptr,
    sync::atomic::{self, AtomicUsize, Ordering},
};

use crossbeam_epoch::Atomic;

/// A lock-free hash map implemented with segmented bucket pointer arrays, open
/// addressing, and linear probing.
///
/// By default, `Cache` uses a hashing algorithm selected to provide resistance
/// against HashDoS attacks.
///
/// The default hashing algorithm is the one used by `std::collections::HashMap`,
/// which is currently SipHash 1-3.
///
/// While its performance is very competitive for medium sized keys, other hashing
/// algorithms will outperform it for small keys such as integers as well as large
/// keys such as long strings. However those algorithms will typically not protect
/// against attacks such as HashDoS.
///
/// The hashing algorithm can be replaced on a per-`HashMap` basis using the
/// [`default`], [`with_hasher`], [`with_capacity_and_hasher`],
/// [`with_num_segments_and_hasher`], and
/// [`with_num_segments_capacity_and_hasher`] methods. Many alternative
/// algorithms are available on crates.io, such as the [`aHash`] crate.
///
/// The number of segments can be specified on a per-`HashMap` basis using the
/// [`with_num_segments`], [`with_num_segments_and_capacity`],
/// [`with_num_segments_and_hasher`], and
/// [`with_num_segments_capacity_and_hasher`] methods. By default, the
/// `num-cpus` feature is enabled and [`new`], [`with_capacity`],
/// [`with_hasher`], and [`with_capacity_and_hasher`] will create maps with
/// twice as many segments as the system has CPUs.
///
/// It is required that the keys implement the [`Eq`] and [`Hash`] traits,
/// although this can frequently be achieved by using
/// `#[derive(PartialEq, Eq, Hash)]`. If you implement these yourself, it is
/// important that the following property holds:
///
/// ```text
/// k1 == k2 -> hash(k1) == hash(k2)
/// ```
///
/// In other words, if two keys are equal, their hashes must be equal.
///
/// It is a logic error for a key to be modified in such a way that the key's
/// hash, as determined by the [`Hash`] trait, or its equality, as determined by
/// the [`Eq`] trait, changes while it is in the map. This is normally only
/// possible through [`Cell`], [`RefCell`], global state, I/O, or unsafe code.
///
/// [`aHash`]: https://crates.io/crates/ahash
/// [`default`]: #method.default
/// [`with_hasher`]: #method.with_hasher
/// [`with_capacity`]: #method.with_capacity
/// [`with_capacity_and_hasher`]: #method.with_capacity_and_hasher
/// [`with_num_segments_and_hasher`]: #method.with_num_segments_and_hasher
/// [`with_num_segments_capacity_and_hasher`]: #method.with_num_segments_capacity_and_hasher
/// [`with_num_segments`]: #method.with_num_segments
/// [`with_num_segments_and_capacity`]: #method.with_num_segments_and_capacity
/// [`new`]: #method.new
/// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
/// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
/// [`Cell`]: https://doc.rust-lang.org/std/cell/struct.Ref.html
/// [`RefCell`]: https://doc.rust-lang.org/std/cell/struct.RefCell.html
pub(crate) struct HashMap<K, V, S = DefaultHashBuilder> {
    segments: Box<[Segment<K, V>]>,
    build_hasher: S,
    len: AtomicUsize,
    segment_shift: u32,
}

#[cfg(test)]
impl<K, V> HashMap<K, V, DefaultHashBuilder> {
    /// Creates an empty `HashMap` with the specified capacity.
    ///
    /// The hash map will be able to hold at least `capacity` elements without
    /// reallocating any bucket pointer arrays. If `capacity` is 0, the hash map
    /// will not allocate any bucket pointer arrays. However, it will always
    /// allocate memory for segment pointers and lengths.
    ///
    /// The `HashMap` will be created with at least twice as many segments as
    /// the system has CPUs.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_num_segments_capacity_and_hasher(
            default_num_segments(),
            capacity,
            DefaultHashBuilder::default(),
        )
    }
}

impl<K, V, S> HashMap<K, V, S> {
    /// Creates an empty `HashMap` with the specified number of segments, using
    /// `build_hasher` to hash the keys.
    ///
    /// The hash map is initially created with a capacity of 0, so it will not
    /// allocate bucket pointer arrays until it is first inserted into. However,
    /// it will always allocate memory for segment pointers and lengths.
    ///
    /// # Panics
    ///
    /// Panics if `num_segments` is 0.
    pub(crate) fn with_num_segments_and_hasher(num_segments: usize, build_hasher: S) -> Self {
        Self::with_num_segments_capacity_and_hasher(num_segments, 0, build_hasher)
    }

    /// Creates an empty `HashMap` with the specified number of segments and
    /// capacity, using `build_hasher` to hash the keys.
    ///
    /// The hash map will be able to hold at least `capacity` elements without
    /// reallocating any bucket pointer arrays. If `capacity` is 0, the hash map
    /// will not allocate any bucket pointer arrays. However, it will always
    /// allocate memory for segment pointers and lengths.
    ///
    /// # Panics
    ///
    /// Panics if `num_segments` is 0.
    pub(crate) fn with_num_segments_capacity_and_hasher(
        num_segments: usize,
        capacity: usize,
        build_hasher: S,
    ) -> Self {
        assert!(num_segments > 0);

        let actual_num_segments = num_segments.next_power_of_two();
        let segment_shift = 64 - actual_num_segments.trailing_zeros();

        let mut segments = Vec::with_capacity(actual_num_segments);

        if capacity == 0 {
            unsafe {
                ptr::write_bytes(segments.as_mut_ptr(), 0, actual_num_segments);
                segments.set_len(actual_num_segments);
            }
        } else {
            let actual_capacity = (capacity * 2 / actual_num_segments).next_power_of_two();

            for _ in 0..actual_num_segments {
                segments.push(Segment {
                    bucket_array: Atomic::new(BucketArray::with_length(0, actual_capacity)),
                    len: AtomicUsize::new(0),
                });
            }
        }

        let segments = segments.into_boxed_slice();

        Self {
            segments,
            build_hasher,
            len: AtomicUsize::new(0),
            segment_shift,
        }
    }

    /// Returns the number of elements in the map.
    ///
    /// # Safety
    ///
    /// This method on its own is safe, but other threads can add or remove
    /// elements at any time.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Returns `true` if the map contains no elements.
    ///
    /// # Safety
    ///
    /// This method on its own is safe, but other threads can add or remove
    /// elements at any time.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of elements the map can hold without reallocating any
    /// bucket pointer arrays.
    ///
    /// Note that all mutating operations except removal will result in a bucket
    /// being allocated or reallocated.
    ///
    /// # Safety
    ///
    /// This method on its own is safe, but other threads can increase the
    /// capacity of each segment at any time by adding elements.
    #[cfg(any(test, feature = "unstable-debug-counters"))]
    pub(crate) fn capacity(&self) -> usize {
        let guard = &crossbeam_epoch::pin();

        self.segments
            .iter()
            .map(|s| s.bucket_array.load_consume(guard))
            .map(|p| unsafe { p.as_ref() })
            .map(|a| a.map(BucketArray::capacity).unwrap_or(0))
            .sum::<usize>()
    }

    #[cfg(test)]
    /// Returns the number of segments in the map.
    pub(crate) fn num_segments(&self) -> usize {
        self.segments.len()
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> HashMap<K, V, S> {
    /// Returns a clone of the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`Hash`] and [`Eq`] on the borrowed form *must* match those for
    /// the key type.
    ///
    /// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
    /// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
    #[inline]
    pub(crate) fn get<Q>(&self, key: &Q, hash: u64) -> Option<V>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
        V: Clone,
    {
        self.get_key_value_and(key, hash, |_, v| v.clone())
    }

    /// Returns a clone of the the key-value pair corresponding to the supplied
    /// key.
    ///
    /// The supplied key may be any borrowed form of the map's key type, but
    /// [`Hash`] and [`Eq`] on the borrowed form *must* match those for the key
    /// type.
    ///
    /// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
    /// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
    #[inline]
    pub(crate) fn get_key_value<Q>(&self, key: &Q, hash: u64) -> Option<(K, V)>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q> + Clone,
        V: Clone,
    {
        self.get_key_value_and(key, hash, |k, v| (k.clone(), v.clone()))
    }

    /// Returns the result of invoking a function with a reference to the
    /// key-value pair corresponding to the supplied key.
    ///
    /// The supplied key may be any borrowed form of the map's key type, but
    /// [`Hash`] and [`Eq`] on the borrowed form *must* match those for the key
    /// type.
    ///
    /// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
    /// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
    #[inline]
    pub(crate) fn get_key_value_and<Q, F, T>(&self, key: &Q, hash: u64, with_entry: F) -> Option<T>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
        F: FnOnce(&K, &V) -> T,
    {
        self.bucket_array_ref(hash)
            .get_key_value_and(key, hash, with_entry)
    }

    /// Inserts a key-value pair into the map, returning the result of invoking
    /// a function with a reference to the key-value pair previously
    /// corresponding to the supplied key.
    ///
    /// If the map did have this key present, both the key and value are
    /// updated.
    #[cfg(test)]
    #[inline]
    pub fn insert_entry_and<F, T>(
        &self,
        key: K,
        hash: u64,
        value: V,
        with_previous_entry: F,
    ) -> Option<T>
    where
        V: Clone,
        F: FnOnce(&K, &V) -> T,
    {
        let result = self
            .bucket_array_ref(hash)
            // .insert_entry_and(key, hash, value, with_previous_entry);
            .insert_with_or_modify_entry_and(
                key,
                hash,
                || value,
                |_k, v| v.clone(),
                with_previous_entry,
            );

        if result.is_none() {
            self.len.fetch_add(1, Ordering::Relaxed);
        }

        result
    }

    /// Removes a key from the map, returning a clone of the value previously
    /// corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`Hash`] and [`Eq`] on the borrowed form *must* match those for
    /// the key type.
    ///
    /// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
    /// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
    #[inline]
    pub(crate) fn remove<Q>(&self, key: &Q, hash: u64) -> Option<V>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
        V: Clone,
    {
        self.remove_entry_if_and(key, hash, |_, _| true, |_, v| v.clone())
    }

    /// Removes a key from the map, returning a clone of the key-value pair
    /// previously corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`Hash`] and [`Eq`] on the borrowed form *must* match those for
    /// the key type.
    ///
    /// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
    /// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
    #[inline]
    pub(crate) fn remove_entry<Q>(&self, key: &Q, hash: u64) -> Option<(K, V)>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q> + Clone,
        V: Clone,
    {
        self.remove_entry_if_and(key, hash, |_, _| true, |k, v| (k.clone(), v.clone()))
    }

    /// Removes a key from the map if a condition is met, returning a clone of
    /// the value previously corresponding to the key.
    ///
    /// `condition` will be invoked at least once if [`Some`] is returned. It
    /// may also be invoked one or more times if [`None`] is returned.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`Hash`] and [`Eq`] on the borrowed form *must* match those for
    /// the key type.
    ///
    /// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
    /// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
    /// [`Some`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.Some
    /// [`None`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.None
    pub(crate) fn remove_if<Q, F>(&self, key: &Q, hash: u64, condition: F) -> Option<V>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
        V: Clone,
        F: FnMut(&K, &V) -> bool,
    {
        self.remove_entry_if_and(key, hash, condition, move |_, v| v.clone())
    }

    /// Removes a key from the map if a condition is met, returning the result
    /// of invoking a function with a reference to the key-value pair previously
    /// corresponding to the key.
    ///
    /// `condition` will be invoked at least once if [`Some`] is returned. It
    /// may also be invoked one or more times if [`None`] is returned.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// [`Hash`] and [`Eq`] on the borrowed form *must* match those for
    /// the key type.
    ///
    /// [`Hash`]: https://doc.rust-lang.org/std/hash/trait.Hash.html
    /// [`Eq`]: https://doc.rust-lang.org/std/cmp/trait.Eq.html
    /// [`Some`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.Some
    /// [`None`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.None
    #[inline]
    pub(crate) fn remove_entry_if_and<Q, F, G, T>(
        &self,
        key: &Q,
        hash: u64,
        condition: F,
        with_previous_entry: G,
    ) -> Option<T>
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
        F: FnMut(&K, &V) -> bool,
        G: FnOnce(&K, &V) -> T,
    {
        self.bucket_array_ref(hash)
            .remove_entry_if_and(key, hash, condition, move |k, v| {
                self.len.fetch_sub(1, Ordering::Relaxed);

                with_previous_entry(k, v)
            })
    }

    /// If no value corresponds to the key, invoke a default function to insert
    /// a new key-value pair into the map. Otherwise, modify the existing value
    /// and return a clone of the value previously corresponding to the key.
    ///
    /// `on_insert` may be invoked, even if [`None`] is returned.
    ///
    /// `on_modify` will be invoked at least once if [`Some`] is returned. It
    /// may also be invoked one or more times if [`None`] is returned.
    ///
    /// [`Some`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.Some
    /// [`None`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.None
    #[inline]
    pub(crate) fn insert_with_or_modify<F, G>(
        &self,
        key: K,
        hash: u64,
        on_insert: F,
        on_modify: G,
    ) -> Option<V>
    where
        V: Clone,
        F: FnOnce() -> V,
        G: FnMut(&K, &V) -> V,
    {
        self.insert_with_or_modify_entry_and(key, hash, on_insert, on_modify, |_, v| v.clone())
    }

    /// If no value corresponds to the key, invoke a default function to insert
    /// a new key-value pair into the map. Otherwise, modify the existing value
    /// and return the result of invoking a function with a reference to the
    /// key-value pair previously corresponding to the supplied key.
    ///
    /// `on_insert` may be invoked, even if [`None`] is returned.
    ///
    /// `on_modify` will be invoked at least once if [`Some`] is returned. It
    /// may also be invoked one or more times if [`None`] is returned.
    ///
    /// [`Some`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.Some
    /// [`None`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.None
    #[inline]
    pub(crate) fn insert_with_or_modify_entry_and<F, G, H, T>(
        &self,
        key: K,
        hash: u64,
        on_insert: F,
        on_modify: G,
        with_old_entry: H,
    ) -> Option<T>
    where
        F: FnOnce() -> V,
        G: FnMut(&K, &V) -> V,
        H: FnOnce(&K, &V) -> T,
    {
        let result = self.bucket_array_ref(hash).insert_with_or_modify_entry_and(
            key,
            hash,
            on_insert,
            on_modify,
            with_old_entry,
        );

        if result.is_none() {
            self.len.fetch_add(1, Ordering::Relaxed);
        }

        result
    }

    #[inline]
    pub(crate) fn insert_if_not_present(&self, key: K, hash: u64, value: V) -> Option<V>
    where
        V: Clone,
    {
        let result = self.bucket_array_ref(hash).insert_if_not_present_and(
            key,
            hash,
            || value,
            |_, v| v.clone(),
        );

        if result.is_none() {
            self.len.fetch_add(1, Ordering::Relaxed);
        }

        result
    }

    #[inline]
    pub(crate) fn hash<Q>(&self, key: &Q) -> u64
    where
        Q: Hash + Eq + ?Sized,
        K: Borrow<Q>,
    {
        bucket::hash(&self.build_hasher, key)
    }
}

impl<K, V, S> Drop for HashMap<K, V, S> {
    fn drop(&mut self) {
        let guard = unsafe { &crossbeam_epoch::unprotected() };
        atomic::fence(Ordering::Acquire);

        for Segment {
            bucket_array: this_bucket_array,
            ..
        } in self.segments.iter()
        {
            let mut current_ptr = this_bucket_array.load(Ordering::Relaxed, guard);

            while let Some(current_ref) = unsafe { current_ptr.as_ref() } {
                let next_ptr = current_ref.next.load(Ordering::Relaxed, guard);

                for this_bucket_ptr in current_ref
                    .buckets
                    .iter()
                    .map(|b| b.load(Ordering::Relaxed, guard))
                    .filter(|p| !p.is_null())
                    .filter(|p| next_ptr.is_null() || p.tag() & bucket::TOMBSTONE_TAG == 0)
                {
                    // only delete tombstones from the newest bucket array
                    // the only way this becomes a memory leak is if there was a panic during a rehash,
                    // in which case I am going to say that running destructors and freeing memory is
                    // best-effort, and my best effort is to not do it
                    unsafe { bucket::defer_acquire_destroy(guard, this_bucket_ptr) };
                }

                unsafe { bucket::defer_acquire_destroy(guard, current_ptr) };

                current_ptr = next_ptr;
            }
        }
    }
}

impl<K, V, S> HashMap<K, V, S> {
    #[inline]
    fn bucket_array_ref(&'_ self, hash: u64) -> BucketArrayRef<'_, K, V, S> {
        let index = self.segment_index_from_hash(hash);

        let Segment {
            ref bucket_array,
            ref len,
        } = self.segments[index];

        BucketArrayRef {
            bucket_array,
            build_hasher: &self.build_hasher,
            len,
        }
    }

    #[inline]
    fn segment_index_from_hash(&'_ self, hash: u64) -> usize {
        if self.segment_shift == 64 {
            0
        } else {
            (hash >> self.segment_shift) as usize
        }
    }
}

struct Segment<K, V> {
    bucket_array: Atomic<BucketArray<K, V>>,
    len: AtomicUsize,
}

#[cfg(test)]
fn default_num_segments() -> usize {
    crate::common::num_cpus() * 2
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier},
        thread::{spawn, JoinHandle},
    };

    use super::*;
    use crate::cht::test_util::{run_deferred, DropNotifier, NoisyDropper};

    #[test]
    fn single_segment() {
        let map =
            HashMap::with_num_segments_capacity_and_hasher(1, 0, DefaultHashBuilder::default());

        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        let key = "foo";
        let hash = map.hash(key);

        assert_eq!(map.insert_entry_and(key, hash, 5, |_, v| *v), None);
        assert_eq!(map.get(key, hash), Some(5));

        assert!(!map.is_empty());
        assert_eq!(map.len(), 1);

        assert_eq!(map.remove(key, hash), Some(5));
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn insert_if_not_present() {
        let map =
            HashMap::with_num_segments_capacity_and_hasher(1, 0, DefaultHashBuilder::default());

        let key = "foo";
        let hash = map.hash(key);

        assert_eq!(map.insert_if_not_present(key, hash, 5), None);
        assert_eq!(map.get(key, hash), Some(5));

        assert_eq!(map.insert_if_not_present(key, hash, 6), Some(5));
        assert_eq!(map.get(key, hash), Some(5));

        assert_eq!(map.remove(key, hash), Some(5));

        assert_eq!(map.insert_if_not_present(key, hash, 7), None);
        assert_eq!(map.get(key, hash), Some(7));

        assert_eq!(map.remove(key, hash), Some(7));
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn insertion() {
        const MAX_VALUE: i32 = 512;

        let map = HashMap::with_capacity(MAX_VALUE as usize);

        for i in 0..MAX_VALUE {
            assert_eq!(map.insert_entry_and(i, map.hash(&i), i, |_, v| *v), None);

            assert!(!map.is_empty());
            assert_eq!(map.len(), (i + 1) as usize);

            for j in 0..=i {
                let hash = map.hash(&j);
                assert_eq!(map.get(&j, hash), Some(j));
                assert_eq!(map.insert_entry_and(j, hash, j, |_, v| *v), Some(j));
            }

            for k in i + 1..MAX_VALUE {
                assert_eq!(map.get(&k, map.hash(&k)), None);
            }
        }

        run_deferred();
    }

    #[test]
    fn growth() {
        const MAX_VALUE: i32 = 512;

        let map = HashMap::with_capacity(0);

        for i in 0..MAX_VALUE {
            assert_eq!(map.insert_entry_and(i, map.hash(&i), i, |_, v| *v), None);

            assert!(!map.is_empty());
            assert_eq!(map.len(), (i + 1) as usize);

            for j in 0..=i {
                let hash = map.hash(&j);
                assert_eq!(map.get(&j, hash), Some(j));
                assert_eq!(map.insert_entry_and(j, hash, j, |_, v| *v), Some(j));
            }

            for k in i + 1..MAX_VALUE {
                assert_eq!(map.get(&k, map.hash(&k)), None);
            }
        }

        run_deferred();
    }

    // Ignore this test and some other tests on 32-bit mips targets to avoid the following
    // error on QEMU user space emulator:
    //
    //     memory allocation of 1052 bytes failed
    //     process didn't exit successfully: ... (signal: 6, SIGABRT: process abort signal)
    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_insertion() {
        const MAX_VALUE: i32 = 512;
        const NUM_THREADS: usize = 64;
        const MAX_INSERTED_VALUE: i32 = (NUM_THREADS as i32) * MAX_VALUE;

        let map = Arc::new(HashMap::with_capacity(MAX_INSERTED_VALUE as usize));
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        #[allow(clippy::needless_collect)]
        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in (0..MAX_VALUE).map(|j| j + (i as i32 * MAX_VALUE)) {
                        assert_eq!(map.insert_entry_and(j, map.hash(&j), j, |_, v| *v), None);
                    }
                })
            })
            .collect();

        for result in threads.into_iter().map(JoinHandle::join) {
            assert!(result.is_ok());
        }

        assert!(!map.is_empty());
        assert_eq!(map.len(), MAX_INSERTED_VALUE as usize);

        for i in 0..MAX_INSERTED_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), Some(i));
        }

        run_deferred();
    }

    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_growth() {
        const MAX_VALUE: i32 = 512;
        const NUM_THREADS: usize = 64;
        const MAX_INSERTED_VALUE: i32 = (NUM_THREADS as i32) * MAX_VALUE;

        let map = Arc::new(HashMap::with_capacity(0));
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        #[allow(clippy::needless_collect)]
        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in (0..MAX_VALUE).map(|j| j + (i as i32 * MAX_VALUE)) {
                        assert_eq!(map.insert_entry_and(j, map.hash(&j), j, |_, v| *v), None);
                    }
                })
            })
            .collect();

        for result in threads.into_iter().map(|t| t.join()) {
            assert!(result.is_ok());
        }

        assert!(!map.is_empty());
        assert_eq!(map.len(), MAX_INSERTED_VALUE as usize);

        for i in 0..MAX_INSERTED_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), Some(i));
        }

        run_deferred();
    }

    #[test]
    fn removal() {
        const MAX_VALUE: i32 = 512;

        let map = HashMap::with_capacity(MAX_VALUE as usize);

        for i in 0..MAX_VALUE {
            assert_eq!(map.insert_entry_and(i, map.hash(&i), i, |_, v| *v), None);
        }

        for i in 0..MAX_VALUE {
            assert_eq!(map.remove(&i, map.hash(&i)), Some(i));
        }

        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        for i in 0..MAX_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), None);
        }

        run_deferred();
    }

    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_removal() {
        const MAX_VALUE: i32 = 512;
        const NUM_THREADS: usize = 64;
        const MAX_INSERTED_VALUE: i32 = (NUM_THREADS as i32) * MAX_VALUE;

        let map = HashMap::with_capacity(MAX_INSERTED_VALUE as usize);

        for i in 0..MAX_INSERTED_VALUE {
            assert_eq!(map.insert_entry_and(i, map.hash(&i), i, |_, v| *v), None);
        }

        let map = Arc::new(map);
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        #[allow(clippy::needless_collect)]
        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in (0..MAX_VALUE).map(|j| j + (i as i32 * MAX_VALUE)) {
                        assert_eq!(map.remove(&j, map.hash(&j)), Some(j));
                    }
                })
            })
            .collect();

        for result in threads.into_iter().map(|t| t.join()) {
            assert!(result.is_ok());
        }

        assert_eq!(map.len(), 0);

        for i in 0..MAX_INSERTED_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), None);
        }

        run_deferred();
    }

    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_insertion_and_removal() {
        const MAX_VALUE: i32 = 512;
        const NUM_THREADS: usize = 64;
        const MAX_INSERTED_VALUE: i32 = (NUM_THREADS as i32) * MAX_VALUE * 2;
        const INSERTED_MIDPOINT: i32 = MAX_INSERTED_VALUE / 2;

        let map = HashMap::with_capacity(MAX_INSERTED_VALUE as usize);

        for i in INSERTED_MIDPOINT..MAX_INSERTED_VALUE {
            assert_eq!(map.insert_entry_and(i, map.hash(&i), i, |_, v| *v), None);
        }

        let map = Arc::new(map);
        let barrier = Arc::new(Barrier::new(NUM_THREADS * 2));

        #[allow(clippy::needless_collect)]
        let insert_threads: Vec<_> = (0..NUM_THREADS)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in (0..MAX_VALUE).map(|j| j + (i as i32 * MAX_VALUE)) {
                        assert_eq!(map.insert_entry_and(j, map.hash(&j), j, |_, v| *v), None);
                    }
                })
            })
            .collect();

        #[allow(clippy::needless_collect)]
        let remove_threads: Vec<_> = (0..NUM_THREADS)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in (0..MAX_VALUE).map(|j| INSERTED_MIDPOINT + j + (i as i32 * MAX_VALUE))
                    {
                        assert_eq!(map.remove(&j, map.hash(&j)), Some(j));
                    }
                })
            })
            .collect();

        for result in insert_threads
            .into_iter()
            .chain(remove_threads.into_iter())
            .map(|t| t.join())
        {
            assert!(result.is_ok());
        }

        assert!(!map.is_empty());
        assert_eq!(map.len(), INSERTED_MIDPOINT as usize);

        for i in 0..INSERTED_MIDPOINT {
            assert_eq!(map.get(&i, map.hash(&i)), Some(i));
        }

        for i in INSERTED_MIDPOINT..MAX_INSERTED_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), None);
        }

        run_deferred();
    }

    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_growth_and_removal() {
        const MAX_VALUE: i32 = 512;
        const NUM_THREADS: usize = 64;
        const MAX_INSERTED_VALUE: i32 = (NUM_THREADS as i32) * MAX_VALUE * 2;
        const INSERTED_MIDPOINT: i32 = MAX_INSERTED_VALUE / 2;

        let map = HashMap::with_capacity(INSERTED_MIDPOINT as usize);

        for i in INSERTED_MIDPOINT..MAX_INSERTED_VALUE {
            assert_eq!(map.insert_entry_and(i, map.hash(&i), i, |_, v| *v), None);
        }

        let map = Arc::new(map);
        let barrier = Arc::new(Barrier::new(NUM_THREADS * 2));

        #[allow(clippy::needless_collect)]
        let insert_threads: Vec<_> = (0..NUM_THREADS)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in (0..MAX_VALUE).map(|j| j + (i as i32 * MAX_VALUE)) {
                        assert_eq!(map.insert_entry_and(j, map.hash(&j), j, |_, v| *v), None);
                    }
                })
            })
            .collect();

        #[allow(clippy::needless_collect)]
        let remove_threads: Vec<_> = (0..NUM_THREADS)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in (0..MAX_VALUE).map(|j| INSERTED_MIDPOINT + j + (i as i32 * MAX_VALUE))
                    {
                        assert_eq!(map.remove(&j, map.hash(&j)), Some(j));
                    }
                })
            })
            .collect();

        for result in insert_threads
            .into_iter()
            .chain(remove_threads.into_iter())
            .map(JoinHandle::join)
        {
            assert!(result.is_ok());
        }

        assert!(!map.is_empty());
        assert_eq!(map.len(), INSERTED_MIDPOINT as usize);

        for i in 0..INSERTED_MIDPOINT {
            assert_eq!(map.get(&i, map.hash(&i)), Some(i));
        }

        for i in INSERTED_MIDPOINT..MAX_INSERTED_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), None);
        }

        run_deferred();
    }

    #[test]
    fn insert_with_or_modify() {
        let map = HashMap::with_capacity(0);

        let key = "foo";
        let hash = map.hash(&key);

        assert_eq!(
            map.insert_with_or_modify(key, hash, || 1, |_, x| x + 1),
            None
        );
        assert_eq!(map.get(key, hash), Some(1));

        assert_eq!(
            map.insert_with_or_modify(key, hash, || 1, |_, x| x + 1),
            Some(1)
        );
        assert_eq!(map.get(key, hash), Some(2));

        run_deferred();
    }

    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_insert_with_or_modify() {
        const NUM_THREADS: usize = 64;
        const MAX_VALUE: i32 = 512;

        let map = Arc::new(HashMap::with_capacity(0));
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        #[allow(clippy::needless_collect)]
        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in 0..MAX_VALUE {
                        map.insert_with_or_modify(j, map.hash(&j), || 1, |_, x| x + 1);
                    }
                })
            })
            .collect();

        for result in threads.into_iter().map(JoinHandle::join) {
            assert!(result.is_ok());
        }

        assert_eq!(map.len(), MAX_VALUE as usize);

        for i in 0..MAX_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), Some(NUM_THREADS as i32));
        }

        run_deferred();
    }

    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_overlapped_insertion() {
        const NUM_THREADS: usize = 64;
        const MAX_VALUE: i32 = 512;

        let map = Arc::new(HashMap::with_capacity(MAX_VALUE as usize));
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        #[allow(clippy::needless_collect)]
        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in 0..MAX_VALUE {
                        map.insert_entry_and(j, map.hash(&j), j, |_, v| *v);
                    }
                })
            })
            .collect();

        for result in threads.into_iter().map(JoinHandle::join) {
            assert!(result.is_ok());
        }

        assert_eq!(map.len(), MAX_VALUE as usize);

        for i in 0..MAX_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), Some(i));
        }

        run_deferred();
    }

    // Ignore this test on 32-bit mips and armv5te targets to avoid the following
    // error on QEMU user space emulator:
    //
    // (mips)
    //     memory allocation of 1052 bytes failed
    //     process didn't exit successfully: ... (signal: 6, SIGABRT: process abort signal)
    //
    // (armv5te)
    //     process didn't exit successfully: ... (signal: 4, SIGILL: illegal instruction)
    //
    #[cfg_attr(any(armv5te, mips), ignore)]
    #[test]
    fn concurrent_overlapped_growth() {
        const NUM_THREADS: usize = 64;
        const MAX_VALUE: i32 = 512;

        let map = Arc::new(HashMap::with_capacity(1));
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        #[allow(clippy::needless_collect)]
        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in 0..MAX_VALUE {
                        map.insert_entry_and(j, map.hash(&j), j, |_, v| *v);
                    }
                })
            })
            .collect();

        for result in threads.into_iter().map(JoinHandle::join) {
            assert!(result.is_ok());
        }

        assert_eq!(map.len(), MAX_VALUE as usize);

        for i in 0..MAX_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), Some(i));
        }

        run_deferred();
    }

    #[cfg_attr(mips, ignore)]
    #[test]
    fn concurrent_overlapped_removal() {
        const NUM_THREADS: usize = 64;
        const MAX_VALUE: i32 = 512;

        let map = HashMap::with_capacity(MAX_VALUE as usize);

        for i in 0..MAX_VALUE {
            map.insert_entry_and(i, map.hash(&i), i, |_, v| *v);
        }

        let map = Arc::new(map);
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        #[allow(clippy::needless_collect)]
        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);

                spawn(move || {
                    barrier.wait();

                    for j in 0..MAX_VALUE {
                        let prev_value = map.remove(&j, map.hash(&j));

                        if let Some(v) = prev_value {
                            assert_eq!(v, j);
                        }
                    }
                })
            })
            .collect();

        for result in threads.into_iter().map(JoinHandle::join) {
            assert!(result.is_ok());
        }

        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        for i in 0..MAX_VALUE {
            assert_eq!(map.get(&i, map.hash(&i)), None);
        }

        run_deferred();
    }

    #[test]
    fn drop_value() {
        let key_parent = Arc::new(DropNotifier::new());
        let value_parent = Arc::new(DropNotifier::new());

        {
            let map = HashMap::with_capacity(0);
            let hash = map.hash(&0);

            assert_eq!(
                map.insert_entry_and(
                    NoisyDropper::new(Arc::clone(&key_parent), 0),
                    hash,
                    NoisyDropper::new(Arc::clone(&value_parent), 0),
                    |_, _| ()
                ),
                None
            );
            assert!(!map.is_empty());
            assert_eq!(map.len(), 1);
            map.get_key_value_and(&0, hash, |_k, v| assert_eq!(v, &0));

            map.remove_entry_if_and(&0, hash, |_, _| true, |_k, v| assert_eq!(v, &0));
            assert!(map.is_empty());
            assert_eq!(map.len(), 0);
            assert_eq!(map.get_key_value_and(&0, hash, |_, _| ()), None);

            run_deferred();

            assert!(!key_parent.was_dropped());
            assert!(value_parent.was_dropped());
        }

        run_deferred();

        assert!(key_parent.was_dropped());
        assert!(value_parent.was_dropped());
    }

    #[test]
    fn drop_many_values() {
        const NUM_VALUES: usize = 1 << 16;

        let key_parents: Vec<_> = std::iter::repeat_with(|| Arc::new(DropNotifier::new()))
            .take(NUM_VALUES)
            .collect();
        let value_parents: Vec<_> = std::iter::repeat_with(|| Arc::new(DropNotifier::new()))
            .take(NUM_VALUES)
            .collect();

        {
            let map = HashMap::with_capacity(0);
            assert!(map.is_empty());
            assert_eq!(map.len(), 0);

            for (i, (this_key_parent, this_value_parent)) in
                key_parents.iter().zip(value_parents.iter()).enumerate()
            {
                assert_eq!(
                    map.insert_entry_and(
                        NoisyDropper::new(Arc::clone(this_key_parent), i),
                        map.hash(&i),
                        NoisyDropper::new(Arc::clone(this_value_parent), i),
                        |_, _| ()
                    ),
                    None
                );

                assert!(!map.is_empty());
                assert_eq!(map.len(), i + 1);
            }

            for i in 0..NUM_VALUES {
                assert_eq!(
                    map.get_key_value_and(&i, map.hash(&i), |k, v| {
                        assert_eq!(*k, i);
                        assert_eq!(*v, i);
                    }),
                    Some(())
                );
            }

            for i in 0..NUM_VALUES {
                assert_eq!(
                    map.remove_entry_if_and(
                        &i,
                        map.hash(&i),
                        |_, _| true,
                        |k, v| {
                            assert_eq!(*k, i);
                            assert_eq!(*v, i);
                        }
                    ),
                    Some(())
                );
            }

            assert!(map.is_empty());
            assert_eq!(map.len(), 0);

            run_deferred();

            let live_key_count =
                NUM_VALUES - key_parents.iter().filter(|k| k.was_dropped()).count();
            let bucket_array_len = map.capacity() * 2;
            assert_eq!(bucket_array_len, map.num_segments() * 128 * 2);
            assert!(live_key_count <= bucket_array_len / 10);

            for this_value_parent in value_parents.iter() {
                assert!(this_value_parent.was_dropped());
            }

            for i in 0..NUM_VALUES {
                assert_eq!(map.get_key_value_and(&i, map.hash(&i), |_, _| ()), None);
            }
        }

        run_deferred();

        for this_key_parent in key_parents.into_iter() {
            assert!(this_key_parent.was_dropped());
        }

        for this_value_parent in value_parents.into_iter() {
            assert!(this_value_parent.was_dropped());
        }
    }

    #[test]
    fn drop_many_values_concurrent() {
        const NUM_THREADS: usize = 64;
        const NUM_VALUES_PER_THREAD: usize = 512;
        const NUM_VALUES: usize = NUM_THREADS * NUM_VALUES_PER_THREAD;

        let key_parents: Arc<Vec<_>> = Arc::new(
            std::iter::repeat_with(|| Arc::new(DropNotifier::new()))
                .take(NUM_VALUES)
                .collect(),
        );
        let value_parents: Arc<Vec<_>> = Arc::new(
            std::iter::repeat_with(|| Arc::new(DropNotifier::new()))
                .take(NUM_VALUES)
                .collect(),
        );

        {
            let map = Arc::new(HashMap::with_capacity(0));
            assert!(map.is_empty());
            assert_eq!(map.len(), 0);

            let barrier = Arc::new(Barrier::new(NUM_THREADS));

            #[allow(clippy::needless_collect)]
            let handles: Vec<_> = (0..NUM_THREADS)
                .map(|i| {
                    let map = Arc::clone(&map);
                    let barrier = Arc::clone(&barrier);
                    let key_parents = Arc::clone(&key_parents);
                    let value_parents = Arc::clone(&value_parents);

                    spawn(move || {
                        barrier.wait();

                        let these_key_parents = &key_parents
                            [i * NUM_VALUES_PER_THREAD..(i + 1) * NUM_VALUES_PER_THREAD];
                        let these_value_parents = &value_parents
                            [i * NUM_VALUES_PER_THREAD..(i + 1) * NUM_VALUES_PER_THREAD];

                        for (j, (this_key_parent, this_value_parent)) in these_key_parents
                            .iter()
                            .zip(these_value_parents.iter())
                            .enumerate()
                        {
                            let key_value = (i * NUM_VALUES_PER_THREAD + j) as i32;
                            let hash = map.hash(&key_value);

                            assert_eq!(
                                map.insert_entry_and(
                                    NoisyDropper::new(Arc::clone(this_key_parent), key_value),
                                    hash,
                                    NoisyDropper::new(Arc::clone(this_value_parent), key_value),
                                    |_, _| ()
                                ),
                                None
                            );
                        }
                    })
                })
                .collect();

            for result in handles.into_iter().map(JoinHandle::join) {
                assert!(result.is_ok());
            }

            assert!(!map.is_empty());
            assert_eq!(map.len(), NUM_VALUES);

            run_deferred();

            for this_key_parent in key_parents.iter() {
                assert!(!this_key_parent.was_dropped());
            }

            for this_value_parent in value_parents.iter() {
                assert!(!this_value_parent.was_dropped());
            }

            for i in (0..NUM_VALUES).map(|i| i as i32) {
                assert_eq!(
                    map.get_key_value_and(&i, map.hash(&i), |k, v| {
                        assert_eq!(*k, i);
                        assert_eq!(*v, i);
                    }),
                    Some(())
                );
            }

            #[allow(clippy::needless_collect)]
            let handles: Vec<_> = (0..NUM_THREADS)
                .map(|i| {
                    let map = Arc::clone(&map);
                    let barrier = Arc::clone(&barrier);

                    spawn(move || {
                        barrier.wait();

                        for j in 0..NUM_VALUES_PER_THREAD {
                            let key_value = (i * NUM_VALUES_PER_THREAD + j) as i32;

                            assert_eq!(
                                map.remove_entry_if_and(
                                    &key_value,
                                    map.hash(&key_value),
                                    |_, _| true,
                                    |k, v| {
                                        assert_eq!(*k, key_value);
                                        assert_eq!(*v, key_value);
                                    }
                                ),
                                Some(())
                            );
                        }
                    })
                })
                .collect();

            for result in handles.into_iter().map(JoinHandle::join) {
                assert!(result.is_ok());
            }

            assert!(map.is_empty());
            assert_eq!(map.len(), 0);

            run_deferred();

            let live_key_count =
                NUM_VALUES - key_parents.iter().filter(|k| k.was_dropped()).count();
            let bucket_array_len = map.capacity() * 2;
            assert_eq!(bucket_array_len, map.num_segments() * 128 * 2);
            assert!(live_key_count <= bucket_array_len / 10);

            for this_value_parent in value_parents.iter() {
                assert!(this_value_parent.was_dropped());
            }

            for i in (0..NUM_VALUES).map(|i| i as i32) {
                assert_eq!(map.get_key_value_and(&i, map.hash(&i), |_, _| ()), None);
            }
        }

        run_deferred();

        for this_key_parent in key_parents.iter() {
            assert!(this_key_parent.was_dropped());
        }

        for this_value_parent in value_parents.iter() {
            assert!(this_value_parent.was_dropped());
        }
    }

    #[test]
    fn remove_if() {
        const NUM_VALUES: i32 = 512;

        let is_even = |_: &i32, v: &i32| *v % 2 == 0;

        let map = HashMap::with_capacity(0);

        for i in 0..NUM_VALUES {
            assert_eq!(map.insert_entry_and(i, map.hash(&i), i, |_, v| *v), None);
        }

        for i in 0..NUM_VALUES {
            if is_even(&i, &i) {
                assert_eq!(map.remove_if(&i, map.hash(&i), is_even), Some(i));
            } else {
                assert_eq!(map.remove_if(&i, map.hash(&i), is_even), None);
            }
        }

        for i in (0..NUM_VALUES).filter(|i| i % 2 == 0) {
            assert_eq!(map.get(&i, map.hash(&i)), None);
        }

        for i in (0..NUM_VALUES).filter(|i| i % 2 != 0) {
            assert_eq!(map.get(&i, map.hash(&i)), Some(i));
        }

        run_deferred();
    }
}
