//! The **arena allocator** (`PLAN.md` Phase 6, §2.4).
//!
//! Every value a script creates during one invocation — strings, arrays, maps, records —
//! lives in a bump arena that is reset *wholesale* when the call returns. There is no GC:
//! short embedded invocations allocate freely and the whole region is reclaimed at once.
//! Persistent data goes through host memory instead (Rust-owned, see [`super::host`]).
//!
//! ## Offsets, not raw pointers (yet)
//!
//! This Phase 6 arena hands out [`ArenaRef`] *offsets* into a backing `Vec<u8>`, so it is
//! plain safe Rust. The (Phase 7) JIT will materialize a real `base + offset` address per
//! allocation; keeping the model offset-based now lets the runtime and its tests exercise
//! the allocation/reset discipline without committing to `unsafe` ahead of the backend.

/// A handle to a region allocated in an [`Arena`]: a byte offset and length. Valid until the
/// next [`Arena::reset`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ArenaRef {
    pub offset: usize,
    pub len: usize,
}

/// A bump allocator reset once per invocation.
///
/// [`Arena::reset`] truncates to empty but **retains capacity**, so steady-state invocations
/// stop allocating from the OS entirely — the same buffer is reused call after call.
pub struct Arena {
    buf: Vec<u8>,
}

impl Arena {
    /// A new, empty arena.
    pub fn new() -> Self {
        Arena { buf: Vec::new() }
    }

    /// A new arena that preallocates `cap` bytes of backing storage.
    pub fn with_capacity(cap: usize) -> Self {
        Arena {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Bytes currently allocated (the bump pointer offset).
    pub fn bytes_used(&self) -> usize {
        self.buf.len()
    }

    /// Backing capacity in bytes (retained across [`reset`](Self::reset)).
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Reclaim every allocation made since construction / the last reset. Keeps capacity so
    /// the next invocation reuses the same memory. Outstanding [`ArenaRef`]s are logically
    /// invalid afterward (this is a per-invocation reset, mirroring the trust model: callers
    /// don't hold arena refs across invocations).
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Allocate `len` zeroed bytes aligned to `align` (a power of two). Returns the region's
    /// handle.
    pub fn alloc(&mut self, len: usize, align: usize) -> ArenaRef {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        let misalign = self.buf.len() & (align - 1);
        if misalign != 0 {
            let pad = align - misalign;
            self.buf.resize(self.buf.len() + pad, 0);
        }
        let offset = self.buf.len();
        self.buf.resize(offset + len, 0);
        ArenaRef { offset, len }
    }

    /// Copy `data` into a freshly allocated, `align`-aligned region and return its handle.
    pub fn alloc_slice(&mut self, data: &[u8], align: usize) -> ArenaRef {
        let r = self.alloc(data.len(), align);
        self.buf[r.offset..r.offset + data.len()].copy_from_slice(data);
        r
    }

    /// Read the bytes of a previously allocated region.
    pub fn bytes(&self, r: ArenaRef) -> &[u8] {
        &self.buf[r.offset..r.offset + r.len]
    }

    /// Mutably access the bytes of a previously allocated region.
    pub fn bytes_mut(&mut self, r: ArenaRef) -> &mut [u8] {
        &mut self.buf[r.offset..r.offset + r.len]
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_advances_and_zeroes() {
        let mut a = Arena::new();
        let r = a.alloc(4, 1);
        assert_eq!(r.offset, 0);
        assert_eq!(r.len, 4);
        assert_eq!(a.bytes(r), &[0, 0, 0, 0]);
        assert_eq!(a.bytes_used(), 4);
    }

    #[test]
    fn alloc_respects_alignment() {
        let mut a = Arena::new();
        a.alloc(1, 1); // offset 0, used = 1
        let r = a.alloc(8, 8); // must pad to offset 8
        assert_eq!(r.offset, 8);
        assert_eq!(a.bytes_used(), 16);
    }

    #[test]
    fn alloc_slice_copies_bytes() {
        let mut a = Arena::new();
        let r = a.alloc_slice(b"hello", 1);
        assert_eq!(a.bytes(r), b"hello");
    }

    #[test]
    fn bytes_mut_allows_writes() {
        let mut a = Arena::new();
        let r = a.alloc(2, 1);
        a.bytes_mut(r).copy_from_slice(&[7, 9]);
        assert_eq!(a.bytes(r), &[7, 9]);
    }

    #[test]
    fn reset_reclaims_but_keeps_capacity() {
        let mut a = Arena::with_capacity(64);
        let cap = a.capacity();
        a.alloc(32, 8);
        assert!(a.bytes_used() > 0);
        a.reset();
        assert_eq!(a.bytes_used(), 0);
        assert_eq!(a.capacity(), cap, "reset must retain capacity");
        // Reused region starts at 0 again.
        assert_eq!(a.alloc(4, 1).offset, 0);
    }
}
