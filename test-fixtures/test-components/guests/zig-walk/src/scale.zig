//! A second file of the guest's own, for stepping from one file into another and back.

const walk = @import("walk.zig");

/// `x` twice over, by way of `add` back in `walk.zig`.
pub fn scale(x: u32) u32 {
    const doubled = walk.add(x, x); // mark: scale-add
    return doubled; // mark: scale-end
}
