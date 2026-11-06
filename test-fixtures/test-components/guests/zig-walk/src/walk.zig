//! A node written in Zig. `bindings/zig_walk.{c,h}` are what `wit-bindgen c` makes of
//! `wit/world.wit`, which Zig calls as it calls any C: `node_run` is called with the inputs,
//! hands its outputs to `node_run_return`, and says it is done.

const std = @import("std");
// The bindings' header, as `zig translate-c` makes it: the build passes it as `c`.
const c = @import("c");
const scale = @import("scale.zig");

pub fn add(a: u32, b: u32) u32 {
    const sum = a +% b; // mark: add-body
    return sum;
} // mark: add-end

// mark: above-walk
fn walk(x: u32) u32 {
    const a = add(x, 1); // mark: walk-first
    const b = add(a, 2); // mark: walk-second
    return b;
} // mark: walk-end

const Point = struct { x: i32, y: f64 };

const Shape = union(enum) { dot, circle: f32, rect: struct { w: u32, h: u32 } };

/// A variable of each kind a debugger has to show. What Zig knows as it compiles is no variable
/// once it runs, so each is a `var`, and its address taken, which says it may change.
fn kinds(seed: u32) u32 {
    const count = seed + 1;
    var negative: i64 = -@as(i64, seed) - 7;
    var ratio: f64 = 1.5;
    var flag = true;
    var letter: u8 = 'x';
    var point = Point{ .x = 3, .y = 2.5 };
    var text: []const u8 = "hello";
    var list = [3]u32{ 10, 20, 30 };
    var slice: []const u32 = &list;
    var some: ?u32 = 7;
    var none: ?u32 = null;
    var shape = Shape{ .rect = .{ .w = 4, .h = 5 } };
    var pointer = &point;
    std.mem.doNotOptimizeAway(.{ &negative, &ratio, &flag, &letter, &text, &slice }); // mark: kinds-used
    std.mem.doNotOptimizeAway(.{ &some, &none, &shape, &pointer });
    return count; // mark: kinds-end
}

export fn node_run(inputs: *c.node_inputs_t) c.zig_walk_callback_code_t {
    var out = walk(if (inputs.in.is_some) inputs.in.val else 0); // mark: run-walk
    if (inputs.kinds.is_some) {
        out +%= kinds(inputs.kinds.val); // mark: run-kinds
    }
    if (inputs.scale.is_some) {
        out +%= scale.scale(inputs.scale.val); // mark: run-scale
    }
    c.node_run_return(.{ .out = out }); // mark: run-end
    return c.ZIG_WALK_CALLBACK_CODE_EXIT;
}

/// `node_run` is done before it returns: it never waits, so is never called back.
export fn node_run_callback(event: *c.zig_walk_event_t) c.zig_walk_callback_code_t {
    _ = event;
    @trap();
}
