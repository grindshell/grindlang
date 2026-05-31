//! Heap **memory layouts** of Grindlang's reference values (`PLAN.md` Phase 6).
//!
//! These describe exactly how a `string`, `array`, `map`, and `record` are laid out in the
//! [arena](super::arena), so the (Phase 7) JIT can emit direct loads/stores against the
//! header fields and payloads. They are *specifications* with `#[repr(C)]` headers and
//! offset/size constants; the reference interpreters still use [`crate::interp::Value`], but
//! the layouts here are what native code will materialize.
//!
//! All payload cells are [`Slot`](super::repr::Slot)-sized (8 bytes), so element and field
//! access is a single shifted load.

use std::collections::BTreeMap;

use crate::types::Type;

use super::repr::Repr;

/// Size of one payload cell (a [`Slot`](super::repr::Slot)) in bytes.
pub const SLOT_SIZE: usize = 8;
/// Alignment of a reference object's header / payload.
pub const OBJECT_ALIGN: usize = 8;

/// Header of a `string`: an immutable, length-prefixed UTF-8 byte buffer. The bytes follow
/// the header inline, starting at [`GSTRING_BYTES_OFFSET`]. Constant strings may be interned
/// (allocated once) by the backend.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GStringHeader {
    /// Number of UTF-8 bytes that follow the header.
    pub len: u32,
}

/// Byte offset of a string's payload bytes from the start of its object.
pub const GSTRING_BYTES_OFFSET: usize = std::mem::size_of::<GStringHeader>();

/// Header of an `array<T>`: a length/capacity pair followed by `cap` payload [`Slot`]s
/// starting at [`GARRAY_DATA_OFFSET`]. Arrays are 1-based at the language level; element `i`
/// lives at payload index `i - 1`. Growth reallocates a larger object in the arena (there is
/// no in-place growth — the arena never frees).
///
/// [`Slot`]: super::repr::Slot
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GArrayHeader {
    /// Number of live elements.
    pub len: u32,
    /// Number of element cells allocated.
    pub cap: u32,
}

/// Byte offset of an array's element payload from the start of its object.
pub const GARRAY_DATA_OFFSET: usize = std::mem::size_of::<GArrayHeader>();

/// Byte offset of array element `index_0` (0-based) from the start of the object.
pub fn array_elem_offset(index_0: usize) -> usize {
    GARRAY_DATA_OFFSET + index_0 * SLOT_SIZE
}

/// Header of a `map<string, T>`: a sorted array of (key, value) entries. Keys are interned
/// string handles; values are payload [`Slot`]s. Entries are kept sorted by key so lookup is
/// a binary search and iteration order is deterministic (matching the interpreters'
/// `BTreeMap` ordering, which the differential tests rely on).
///
/// The entry payload (`len` interleaved key/value cells) starts at [`GMAP_DATA_OFFSET`].
///
/// [`Slot`]: super::repr::Slot
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GMapHeader {
    /// Number of entries.
    pub len: u32,
    /// Number of entry slots allocated.
    pub cap: u32,
}

/// Byte offset of a map's entry payload from the start of its object.
pub const GMAP_DATA_OFFSET: usize = std::mem::size_of::<GMapHeader>();
/// Bytes per map entry: one key handle cell + one value cell.
pub const GMAP_ENTRY_SIZE: usize = 2 * SLOT_SIZE;

/// A `record` has statically known fields, so it needs **no header and no keys at runtime**:
/// it is just a flat tuple of payload [`Slot`]s, one per field, in sorted-key order (the
/// order [`Type::Record`] stores them in a `BTreeMap`). A field read compiles to a constant
/// offset load. This computes the byte offset of `field` within such a record.
///
/// Returns `None` if `field` is not part of the record.
///
/// [`Slot`]: super::repr::Slot
pub fn record_field_offset(fields: &BTreeMap<String, Type>, field: &str) -> Option<usize> {
    fields
        .keys()
        .position(|k| k == field)
        .map(|i| i * SLOT_SIZE)
}

/// Total size in bytes of a record value (one [`Slot`](super::repr::Slot) per field).
pub fn record_size(fields: &BTreeMap<String, Type>) -> usize {
    fields.len() * SLOT_SIZE
}

/// The [`Repr`] of a record field, if present.
pub fn record_field_repr(fields: &BTreeMap<String, Type>, field: &str) -> Option<Repr> {
    fields.get(field).map(Repr::of)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_layout() {
        // u32 length header, bytes immediately after.
        assert_eq!(std::mem::size_of::<GStringHeader>(), 4);
        assert_eq!(GSTRING_BYTES_OFFSET, 4);
    }

    #[test]
    fn array_layout() {
        assert_eq!(std::mem::size_of::<GArrayHeader>(), 8);
        assert_eq!(GARRAY_DATA_OFFSET, 8);
        assert_eq!(array_elem_offset(0), 8);
        assert_eq!(array_elem_offset(3), 8 + 24);
    }

    #[test]
    fn map_layout() {
        assert_eq!(std::mem::size_of::<GMapHeader>(), 8);
        assert_eq!(GMAP_DATA_OFFSET, 8);
        assert_eq!(GMAP_ENTRY_SIZE, 16);
    }

    #[test]
    fn record_offsets_are_sorted_field_order() {
        let mut fields = BTreeMap::new();
        fields.insert("hp".to_string(), Type::Number);
        fields.insert("alive".to_string(), Type::Bool);
        fields.insert("name".to_string(), Type::String);
        // BTreeMap order: alive, hp, name.
        assert_eq!(record_field_offset(&fields, "alive"), Some(0));
        assert_eq!(record_field_offset(&fields, "hp"), Some(SLOT_SIZE));
        assert_eq!(record_field_offset(&fields, "name"), Some(2 * SLOT_SIZE));
        assert_eq!(record_field_offset(&fields, "missing"), None);
        assert_eq!(record_size(&fields), 3 * SLOT_SIZE);
        assert_eq!(record_field_repr(&fields, "alive"), Some(Repr::Bool));
        assert_eq!(record_field_repr(&fields, "hp"), Some(Repr::Number));
    }
}
