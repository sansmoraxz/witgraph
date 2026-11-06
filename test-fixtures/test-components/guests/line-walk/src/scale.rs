//! A second file of the guest's own, for stepping from one file into another and back.

/// `x` twice over, by way of `add` back in `lib.rs`.
pub fn scale(x: u32) -> u32 {
    let doubled = crate::add(x, x); // mark: scale-add
    core::hint::black_box(doubled) // mark: scale-end
}
