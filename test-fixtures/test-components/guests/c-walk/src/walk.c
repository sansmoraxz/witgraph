// A node written in C. `bindings/c_walk.{c,h}` are what `wit-bindgen c` makes of
// `wit/world.wit`: `node_run` is called with the inputs, hands its outputs to
// `node_run_return`, and says it is done.

#include "c_walk.h"
#include "clamp.h"

static uint32_t add(uint32_t a, uint32_t b) {
  uint32_t sum = a + b; // mark: add-body
  return sum;
} // mark: add-end

// mark: above-walk
static uint32_t walk(uint32_t x) {
  uint32_t a = add(x, 1); // mark: walk-first
  uint32_t b = add(a, 2); // mark: walk-second
  return b;
} // mark: walk-end

struct point {
  int32_t x;
  double y;
};

enum shape { DOT, CIRCLE, RECT };

union bits {
  float f;
  uint32_t u;
};

/// A variable of each kind a debugger has to show.
static uint32_t kinds(uint32_t seed) {
  uint32_t count = seed + 1;
  int64_t negative = -(int64_t)seed - 7;
  double ratio = 1.5;
  bool flag = true;
  char letter = 'x';
  struct point point = {.x = 3, .y = 2.5};
  const char *text = "hello";
  uint32_t list[3] = {10, 20, 30};
  enum shape shape = RECT;
  union bits bits = {.f = 1.0f};
  struct point *pointer = &point;
  __asm__ volatile("" ::"r"(&negative), "r"(&ratio), "r"(&flag), "r"(&letter)); // mark: kinds-used
  __asm__ volatile("" ::"r"(&text), "r"(list), "r"(&shape), "r"(&bits), "r"(&pointer));
  return count; // mark: kinds-end
}

c_walk_callback_code_t node_run(node_inputs_t *inputs) {
  uint32_t out = walk(inputs->in.is_some ? inputs->in.val : 0); // mark: run-walk
  if (inputs->kinds.is_some) {
    out += kinds(inputs->kinds.val); // mark: run-kinds
  }
  out = clamp_u32(out, 1, UINT32_MAX); // mark: run-clamp
  node_run_return((node_outputs_t){.out = out}); // mark: run-end
  return C_WALK_CALLBACK_CODE_EXIT;
}

// `node_run` is done before it returns: it never waits, so is never called back.
c_walk_callback_code_t node_run_callback(c_walk_event_t *event) {
  (void)event;
  __builtin_trap();
}
