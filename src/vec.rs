use std::alloc::{self, Layout};
use std::borrow::Borrow;
use std::cmp::{self, Ordering};
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering::*};

/// Create a new [`EcoVec`] from a format string.
#[macro_export]
macro_rules! eco_vec {
    () => { $crate::EcoVec::new() };
    ($elem:expr; $n:expr) => { $crate::EcoVec::from_elem($elem, $n) };
    ($($value:expr),+ $(,)?) => {{
        let capacity = 0 $(+ $crate::eco_vec!(@count $value))*;
        let mut vec = $crate::EcoVec::with_capacity(capacity);
        $(vec.push($value);)*
        vec
    }};
    (@count $value:expr) => { 1 };
}

/// An economical vector with clone-on-write semantics.
///
/// Has a size of one word and is null-pointer optimized (meaning that
/// `Option<EcoVec<T>>` also takes only one word).
///
/// Most mutating methods require `T: Clone` due to clone-on-write semantics.
pub struct EcoVec<T> {
    /// Must always point to a valid header.
    ///
    /// This may point to the `EMPTY` header if the vector is empty. Then, it is
    /// aligned to the header's alignment and may not be mutated.
    ///
    /// Otherwise, this points to a backing allocation. Then, it is aligned to
    /// the maximum of the header's and T's alignment and may be mutated. (At
    /// least the reference-count. To mutate length or capacity, we additionally
    /// require the reference-count to be `1`.)
    ///
    /// The `ptr` offset by `Self::offset()` is valid for `len` reads and
    /// `capacity` writes, but if it points to the `EMPTY` header, it may to be
    /// aligned to T's alignment.
    ptr: NonNull<Header>,
    /// For variance.
    phantom: PhantomData<T>,
}

/// The start of the data.
///
/// This is followed by padding, if necessary, and then the actual data.
#[derive(Debug)]
struct Header {
    /// The vector's reference count. Starts at 1 and only drops to zero
    /// when the last vector is dropped.
    ///
    /// Invariant: `refs <= isize::MAX`.
    refs: AtomicUsize,
    /// The number of elements in the vector.
    ///
    /// May only be mutated if `refs == 1`.
    ///
    /// Invariant: `len <= capacity`.
    len: usize,
    /// The number of elements the backing allocation can hold. Zero if there
    /// is no backing allocation.
    ///
    /// May only be mutated if `refs == 1`.
    ///
    /// Invariant: `capacity <= isize::MAX`.
    capacity: usize,
}

/// The header used for a vector without any items.
static EMPTY: Header = Header { refs: AtomicUsize::new(1), len: 0, capacity: 0 };

impl<T> EcoVec<T> {
    /// Create a new, empty vector.
    pub fn new() -> Self {
        Self {
            // Safety:
            // The `ptr` may be the `EMPTY` header if the vector is empty. The
            // address of `EMPTY` is naturally aligned to the header's
            // alignment.
            ptr: unsafe {
                NonNull::new_unchecked(&EMPTY as *const Header as *mut Header)
            },
            phantom: PhantomData,
        }
    }

    /// Create a new, empty vec with at least the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let mut vec = Self::new();
        if capacity > 0 {
            unsafe {
                // Safety:
                // - The reference count starts at 1.
                // - The capacity starts at 0 and the target capacity is checked
                //   to be `> 0`.
                vec.grow(capacity);
            }
        }
        vec
    }

    /// Returns `true` if the vector contains no elements.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of elements in the vector.
    pub fn len(&self) -> usize {
        self.header().len
    }

    /// How many elements the vector's backing allocation can hold.
    ///
    /// Even if `len < capacity`, pushing into the vector may still
    /// allocate if the reference count is larger than one.
    pub fn capacity(&self) -> usize {
        self.header().capacity
    }

    /// Extracts a slice containing the entire vector.
    pub fn as_slice(&self) -> &[T] {
        // Safety:
        // - The pointer returned by `data()` is non-null, well-aligned, and
        //   valid for `len` reads of `T`.
        // - We have the invariants `len <= capacity <= isize::MAX`.
        // - The memory referenced by the slice isn't mutated for the returned
        //   slice's lifetime, because `self` becomes borrowed and even if there
        //   are other vectors referencing the same backing allocation, they are
        //   now allowed to mutate the slice since the ref-count would be larger
        //   than one.
        unsafe { std::slice::from_raw_parts(self.data(), self.len()) }
    }

    /// Removes all values from the vector.
    pub fn clear(&mut self) {
        // Nothing to do if it's empty.
        if self.is_empty() {
            return;
        }

        // If there are other vectors that reference the same backing
        // allocation, we just create a new, empty vector.
        if self.is_shared() {
            // If another vector was dropped in the meantime, this vector could
            // have become unique, but we don't care, creating a new one
            // is safe nonetheless. Note that this runs the vector's drop
            // impl and reduces the ref-count.
            *self = Self::new();
            return;
        }

        unsafe {
            // Safety:
            // - The vector isn't empty (just checked).
            // - We have unique ownership of the backing allocation, so we can
            //   keep it and clear it. In particular, no other vector can have
            //   gained shared ownership in the meantime since `is_shared()`,
            //   since this is the only live vector available for cloning and we
            //   hold a mutable reference to it.
            self.header_mut().len = 0;
            ptr::drop_in_place(self.as_mut_slice_unchecked());
        }
    }
}

impl<T: Clone> EcoVec<T> {
    /// Create a new vector with `n` copies of `value`.
    pub fn from_elem(value: T, n: usize) -> Self {
        let mut vec = Self::with_capacity(n);
        for _ in 0..n {
            vec.push(value.clone());
        }
        vec
    }

    /// Produce a mutable slice containing the entire vector.
    ///
    /// Clones the vector if its reference count is larger than 1.
    pub fn make_mut(&mut self) -> &mut [T] {
        // To provide mutable access, we must have unique ownership of the
        // backing allocation.
        self.make_unique();

        // Safety:
        // The reference count is `1` because of `make_unique`.
        unsafe { self.as_mut_slice_unchecked() }
    }

    /// Add a value at the end of the vector.
    ///
    /// Clones the vector if its reference count is larger than 1.
    pub fn push(&mut self, value: T) {
        // Grow the vector if necessary and ensure unique ownership.
        let len = self.len();
        if len == self.capacity() {
            self.reserve(1);
        } else {
            self.make_unique();
        }

        // Safety:
        // The reference count is `1` because either `reserve` or `make_unique`
        // has been called.
        unsafe {
            // Safety:
            // - The vector has a backing allocation because if `len` was `0`,
            //   `reserve` will have been called.
            // - Since we reserved space, we maintain `len <= capacity`.
            self.header_mut().len += 1;

            // Safety:
            // - The pointer returned by `data_mut()` is valid for `capacity`
            //   writes.
            // - Due to the check above, `len < capacity`.
            // - Thus, `data_mut() + len` is valid for one write.
            ptr::write(self.data_mut().add(len), value);
        }
    }

    /// Removes the last element from a vector and returns it, or `None` if the
    /// vector is empty.
    ///
    /// Clones the vector if its reference count is larger than 1.
    pub fn pop(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        self.make_unique();

        // Safety:
        // The reference count is `1` because of `make_unique`.
        unsafe {
            // Safety:
            // - The vector has a backing allocation because `is_empty` returned
            //   `false`.
            // - Cannot underflow because we `is_empty` returned `false`.
            let header = self.header_mut();
            let shrunk = header.len - 1;
            header.len = shrunk;

            // Safety:
            // - The pointer returned by `data()` is valid for `len` reads and
            //   thus `data() + new_len` is valid for one read.
            Some(ptr::read(self.data().add(shrunk)))
        }
    }

    /// Inserts an element at position index within the vector, shifting all
    /// elements after it to the right.
    ///
    /// Clones the vector if its reference count is larger than 1.
    ///
    /// Panics if `index > len`.
    pub fn insert(&mut self, index: usize, value: T) {
        let len = self.len();
        if index > len {
            out_of_bounds(index, len);
        }

        // Grow the vector if necessary and ensure unique ownership.
        if len == self.capacity() {
            self.reserve(1);
        } else {
            self.make_unique();
        }

        // Safety:
        // The reference count is `1` because either `reserve` or `make_unique`
        // has been called.
        unsafe {
            // Safety:
            // - The vector has a backing allocation because if `len` was `0`,
            //   `reserve` will have been called.
            // - Since we reserved space, we maintain `len <= capacity`.
            self.header_mut().len += 1;

            // Safety:
            // - The pointer returned by `data_mut()` is valid for `len`
            //   reads and `capacity` writes of `T`.
            // - Thus, `at` is valid for `len - index` reads of `T`
            // - And `at` is valid for `capacity - index` writes of `T`.
            //   Because of the `reserve` call, we have `len < capacity` and
            //   thus `at + 1` is valid for `len - index` writes of `T`.
            let at = self.data_mut().add(index);
            ptr::copy(at, at.add(1), len - index);

            // Safety:
            // - The pointer returned by `data_mut()` is valid for `capacity`
            //   writes.
            // - Due to the bounds check above, `index <= len`
            // - Due to the reserve check, `len < capacity`.
            // - Thus, `data() + index` is valid for one write.
            ptr::write(at, value);
        }
    }

    /// Removes and returns the element at position index within the vector,
    /// shifting all elements after it to the left.
    ///
    /// Clones the vector if its reference count is larger than 1.
    ///
    /// Panics if `index >= len`.
    pub fn remove(&mut self, index: usize) -> T {
        let len = self.len();
        if index >= len {
            out_of_bounds(index, len);
        }

        self.make_unique();

        // Safety:
        // The reference count is `1` because of `make_unique`.
        unsafe {
            // Safety:
            // - The vector has a backing allocation because if `len` was `0`,
            //   the removal would be out of bounds.
            // - Cannot underflow because `index < len` and thus `len > 0`.
            self.header_mut().len -= 1;

            // Safety:
            // - The pointer returned by `data()` is valid for `len` reads.
            // - Due to the check above, `index < len`.
            // - Thus, `at` is valid for one read.
            let at = self.data_mut().add(index);
            let val = ptr::read(at);

            // Safety:
            // - The pointer returned by `data()` is valid for `len` reads and
            //   `capacity` writes.
            // - Thus, `at + 1` is valid for `len - index - 1` reads.
            // - Thus, `at` is valid for `capacity - index` writes.
            // - Due to the invariant `len <= capacity`, `at` is also valid
            //   for `len - index - 1` writes.
            ptr::copy(at.add(1), at, len - index - 1);

            val
        }
    }

    /// Shortens the vector, keeping the first len elements and dropping the
    /// rest.
    ///
    /// Clones the vector if its reference count is larger than 1 and
    /// `target < len`.
    pub fn truncate(&mut self, target: usize) {
        let len = self.len();
        if target >= len {
            return;
        }

        self.make_unique();

        // Safety:
        // The reference count is `1` because of `make_unique`.
        unsafe {
            // Safety:
            // - The vector has a backing allocation because `target < len`
            //   and thus `len > 0`.
            // - Since `target < len`, we maintain `len < capacity`.
            self.header_mut().len = target;

            // Safety:
            // - The pointer returned by `data_mut()` is valid for `capacity`
            //   writes.
            // - We have the invariant `len <= capacity`.
            // - Thus, `data_mut() + target` is valid for `len - target` writes.
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                self.data_mut().add(target),
                len - target,
            ));
        }
    }

    /// Reserve space for at least `additional` more elements.
    ///
    /// Rellocates if the the current capacity isn't sufficient or if the
    /// vector's reference count is larger than 1.
    pub fn reserve(&mut self, additional: usize) {
        self.make_unique();

        let len = self.len();
        let capacity = self.capacity();
        if additional > capacity - len {
            // Reserve at least the `additional` capacity, but also at least
            // double the capacity to ensure exponential growth and finally
            // jump directly to a minimum capacity to prevent frequent
            // reallocation for small vectors.
            let target = len
                .checked_add(additional)
                .unwrap_or_else(|| capacity_overflow())
                .max(2 * capacity)
                .max(Self::min_cap());

            unsafe {
                // Safety:
                // - The reference count is `1` because of `make_unique`.
                // - The `target` capacity is greater than the current capacity
                //   because `additional > 0`.
                self.grow(target);
            }
        }
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// Clones the vector if its reference count is larger than 1.
    ///
    /// Note that this clones the vector even if `f` always returns `false`. To
    /// prevent that, you can first iterate over the vector yourself and then
    /// only call `retain` if your condition is `false` for some element.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut T) -> bool,
    {
        let len = self.len();
        let values = self.make_mut();

        let mut del = 0;
        for i in 0..len {
            if !f(&mut values[i]) {
                del += 1;
            } else if del > 0 {
                values.swap(i - del, i);
            }
        }

        if del > 0 {
            self.truncate(len - del);
        }
    }
}

impl<T> EcoVec<T> {
    /// Grow the capacity to at least the `target` size.
    ///
    /// May only be called if:
    /// - the reference count is `1`, and
    /// - `target > capacity` (i.e., this methods grows, it doesn't shrink).
    unsafe fn grow(&mut self, mut target: usize) {
        debug_assert!(target > self.capacity());

        // Maintain the `capacity <= isize::MAX` invariant.
        if target > isize::MAX as usize {
            capacity_overflow();
        }

        // Directly go to maximum capacity for ZSTs.
        if std::mem::size_of::<T>() == 0 {
            target = isize::MAX as usize;
        }

        let len = self.len();
        let layout = Self::layout(target);
        let ptr = if !self.is_allocated() {
            // Safety:
            // The layout has non-zero size because `target > 0`.
            alloc::alloc(layout)
        } else {
            // Safety:
            // - `self.ptr` was allocated before (just checked)
            // - the old block was allocated with the current capacity
            // - `Self::size()` guarantees to return a value that is `> 0`
            //   and rounded up to the nearest multiple of `Self::align()`
            //   does not overflow `isize::MAX`.
            alloc::realloc(
                self.ptr.as_ptr() as *mut u8,
                Self::layout(self.capacity()),
                Self::size(target),
            )
        };

        // If non-null the pointer points to a valid allocation.
        self.ptr = NonNull::new(ptr as *mut Header)
            .unwrap_or_else(|| alloc::handle_alloc_error(layout));

        // Safety:
        // The freshly allocated pointer is valid for a write of the header.
        ptr::write(
            self.ptr.as_ptr(),
            Header { refs: AtomicUsize::new(1), len, capacity: target },
        );
    }

    /// Extracts a mutable slice containing the entire vector without checking
    /// the reference count.
    ///
    /// May only be called if the reference count is `1`.
    unsafe fn as_mut_slice_unchecked(&mut self) -> &mut [T] {
        debug_assert!(!self.is_shared());
        std::slice::from_raw_parts_mut(self.data_mut(), self.len())
    }

    /// A reference to the header.
    fn header(&self) -> &Header {
        // Safety: The pointer always points to a valid header, even if the
        // vector is empty.
        unsafe { self.ptr.as_ref() }
    }

    /// A mutable reference to the header.
    ///
    /// May only be called if:
    /// - the reference count is `1`, and
    /// - `is_allocated` or `!is_empty`
    unsafe fn header_mut(&mut self) -> &mut Header {
        // Safety: The pointer always points to a valid header.
        debug_assert!(self.is_allocated());
        self.ptr.as_mut()
    }

    /// Whether this vector has a backing allocation.
    fn is_allocated(&self) -> bool {
        self.ptr.as_ptr() as *const Header != &EMPTY as *const Header
    }

    /// Whether another instance is pointing to the same backing allocation.
    fn is_shared(&self) -> bool {
        self.header().refs.load(Relaxed) > 1
    }

    /// The data pointer.
    ///
    /// Returns a pointer that is non-null, well-aligned, and valid for `len`
    /// reads of `T`.
    fn data(&self) -> *const T {
        // When `T` has bigger alignment than the header, the `ptr` may not be
        // well-aligned. However, then `len` is guaranteed to be `0`, so we can
        // just hand out a well-aligned dangling pointer.
        if mem::align_of::<T>() > mem::align_of::<Header>() && !self.is_allocated() {
            return NonNull::dangling().as_ptr();
        }

        // Safety:
        // The `ptr` is non-null, well-aligned, and offset by `Self::offset()`
        // it is valid `len` reads of `T`.
        unsafe {
            let ptr = self.ptr.as_ptr() as *const u8;
            ptr.add(Self::offset()) as *const T
        }
    }

    /// The data pointer, mutably.
    ///
    /// Returns a pointer that is non-null, well-aligned, and valid for
    /// `capacity` writes of `T`.
    fn data_mut(&mut self) -> *mut T {
        // See the explanation above.
        if mem::align_of::<T>() > mem::align_of::<Header>() && !self.is_allocated() {
            return NonNull::dangling().as_ptr();
        }

        // Safety:
        // The `ptr` is non-null, well-aligned, and offset by `Self::offset()`
        // it is valid `capacity` writes of `T`.
        unsafe {
            let ptr = self.ptr.as_ptr() as *mut u8;
            ptr.add(Self::offset()) as *mut T
        }
    }

    /// The layout of a backing allocation for the given capacity.
    fn layout(capacity: usize) -> Layout {
        // Safety:
        // - `Self::size(capacity)` guarantees that it rounded up the alignment
        //   does not overflow `isize::MAX`.
        // - Since `Self::align()` is the header's alignment or T's alignment,
        //   it fulfills the requirements of a valid alignment.
        unsafe { Layout::from_size_align_unchecked(Self::size(capacity), Self::align()) }
    }

    /// The size of a backing allocation for the given capacity.
    ///
    /// Always `> 0`. When rounded up to the next multiple of `Self::align()` is
    /// guaranteed to be `<= isize::MAX`.
    fn size(capacity: usize) -> usize {
        mem::size_of::<T>()
            .checked_mul(capacity)
            .and_then(|size| Self::offset().checked_add(size))
            .filter(|&size| {
                // See `Layout::max_size_for_align` for details.
                size <= isize::MAX as usize - Self::align() - 1
            })
            .unwrap_or_else(|| capacity_overflow())
    }

    /// The alignment of the backing allocation.
    fn align() -> usize {
        cmp::max(mem::align_of::<Header>(), mem::align_of::<T>())
    }

    /// The offset of the data in the backing allocation.
    fn offset() -> usize {
        cmp::max(mem::size_of::<Header>(), Self::align())
    }

    /// The minimum non-zero capacity.
    fn min_cap() -> usize {
        // In the spirit of the `EcoVec`, we choose the cutoff size of T from
        // which 1 is the minimum capacity a bit lower than a standard `Vec`.
        if mem::size_of::<T>() == 1 {
            8
        } else if mem::size_of::<T>() <= 32 {
            4
        } else {
            1
        }
    }
}

impl<T: Clone> EcoVec<T> {
    /// Ensure that this vector has a unique backing allocation.
    fn make_unique(&mut self) {
        if self.is_shared() {
            *self = self.iter().cloned().collect();
        }
    }
}

// Safety: Works like `Arc`.
unsafe impl<T: Sync> Sync for EcoVec<T> {}
unsafe impl<T: Send> Send for EcoVec<T> {}

impl<T> Drop for EcoVec<T> {
    fn drop(&mut self) {
        // Nothing to do if there is no backing allocation.
        if !self.is_allocated() {
            return;
        }

        // Drop our ref-count. If there was more than one vector before
        // (including this one), we shouldn't deallocate.
        if self.header().refs.fetch_sub(1, Release) > 1 {
            return;
        }

        unsafe {
            // Safety: No other vector references the backing allocation
            // (just checked) and given that, `as_mut_slice_unchecked` returns
            // a valid slice of initialized values.
            ptr::drop_in_place(self.as_mut_slice_unchecked());

            // Safety: The vector isn't empty, so `self.ptr` points to an
            // allocation with the layout of current capacity.
            alloc::dealloc(self.ptr.as_ptr() as *mut u8, Self::layout(self.capacity()));
        }
    }
}

impl<T> Deref for EcoVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T> Borrow<[T]> for EcoVec<T> {
    fn borrow(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> AsRef<[T]> for EcoVec<T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> Default for EcoVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Debug> Debug for EcoVec<T> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.as_slice().fmt(f)
    }
}

impl<T: Clone> Clone for EcoVec<T> {
    fn clone(&self) -> Self {
        // If the vector has a backing allocation, bump the ref-count.
        if self.is_allocated() {
            // See Arc's clone impl for details about ordering and aborting.
            let prev = self.header().refs.fetch_add(1, Relaxed);
            if prev > isize::MAX as usize {
                std::process::abort();
            }
        }

        Self { ptr: self.ptr, phantom: PhantomData }
    }
}

impl<T: Hash> Hash for EcoVec<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl<T: Eq> Eq for EcoVec<T> {}

impl<T: PartialEq> PartialEq for EcoVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: PartialEq> PartialEq<[T]> for EcoVec<T> {
    fn eq(&self, other: &[T]) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq, const N: usize> PartialEq<[T; N]> for EcoVec<T> {
    fn eq(&self, other: &[T; N]) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq> PartialEq<Vec<T>> for EcoVec<T> {
    fn eq(&self, other: &Vec<T>) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq> PartialEq<EcoVec<T>> for [T] {
    fn eq(&self, other: &EcoVec<T>) -> bool {
        self == other.as_slice()
    }
}

impl<T: PartialEq, const N: usize> PartialEq<EcoVec<T>> for [T; N] {
    fn eq(&self, other: &EcoVec<T>) -> bool {
        self == other.as_slice()
    }
}

impl<T: PartialEq> PartialEq<EcoVec<T>> for Vec<T> {
    fn eq(&self, other: &EcoVec<T>) -> bool {
        self == other.as_slice()
    }
}

impl<T: Ord> Ord for EcoVec<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_slice().cmp(&other.as_slice())
    }
}

impl<T: PartialOrd> PartialOrd for EcoVec<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_slice().partial_cmp(&other.as_slice())
    }
}

impl<T: Clone> FromIterator<T> for EcoVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut vec = Self::new();
        vec.extend(iter);
        vec
    }
}

impl<T: Clone> Extend<T> for EcoVec<T> {
    fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = T>,
    {
        let iter = iter.into_iter();
        let hint = iter.size_hint().0;
        if hint > 0 {
            self.reserve(hint);
        }
        for value in iter {
            self.push(value);
        }
    }
}

impl<T: Clone> From<&[T]> for EcoVec<T> {
    fn from(slice: &[T]) -> Self {
        slice.iter().cloned().collect()
    }
}

impl<T: Clone> From<Vec<T>> for EcoVec<T> {
    /// This needs to allocate to change the layout.
    fn from(vec: Vec<T>) -> Self {
        vec.into_iter().collect()
    }
}

#[cold]
fn capacity_overflow() -> ! {
    panic!("capacity overflow");
}

#[cold]
fn out_of_bounds(index: usize, len: usize) -> ! {
    panic!("index is out bounds (index: {index}, len: {len})");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vec() {
        let mut vec: EcoVec<&'static str> =
            "hello, world! what's going on?".split_whitespace().collect();

        assert_eq!(vec.len(), 5);
        assert_eq!(vec.capacity(), 8);
        assert_eq!(vec, ["hello,", "world!", "what's", "going", "on?"]);
        assert_eq!(vec.pop(), Some("on?"));
        assert_eq!(vec.len(), 4);
        assert_eq!(vec.last(), Some(&"going"));
        assert_eq!(vec.remove(1), "world!");
        assert_eq!(vec.len(), 3);
        assert_eq!(vec, ["hello,", "what's", "going"]);
        assert_eq!(vec[1], "what's");
        vec.push("where?");
        vec.insert(1, "wonder!");
        assert_eq!(vec, ["hello,", "wonder!", "what's", "going", "where?"]);
        vec.retain(|s| s.starts_with("w"));
        assert_eq!(vec, ["wonder!", "what's", "where?"]);
        vec.truncate(1);
        assert_eq!(vec.last(), vec.first());
    }

    #[test]
    fn test_vec_macro() {
        assert_eq!(eco_vec![Box::new(1); 3], vec![Box::new(1); 3]);
    }

    #[test]
    fn test_vec_clone() {
        let mut vec = EcoVec::new();
        vec.push(1);
        vec.push(2);
        vec.push(3);
        assert_eq!(vec.len(), 3);
        let mut vec2 = vec.clone();
        vec2.push(4);
        assert_eq!(vec2.len(), 4);
        assert_eq!(vec2, [1, 2, 3, 4]);
        assert_eq!(vec, [1, 2, 3]);
    }
}
