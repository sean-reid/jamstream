//! Proves render() touches the heap only while the roster changes: after
//! the first frame warms the static scene, steady-state frames are
//! allocation free. Same counting-allocator approach as engine drift tests.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::{H, W, roster};
use jamstream_broadcast::{Renderer, SceneConfig};

struct CountingAlloc;

thread_local! {
    static HEAP_OPS: Cell<u64> = const { Cell::new(0) };
}

fn heap_ops() -> u64 {
    HEAP_OPS.with(Cell::get)
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP_OPS.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP_OPS.with(|c| c.set(c.get() + 1));
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        HEAP_OPS.with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

#[test]
fn render_is_allocation_free_after_warmup() {
    let mut r = Renderer::new(SceneConfig::default());
    let mut members = roster(10);
    let mut out = vec![0u8; (W * H * 4) as usize];

    // Warmup: builds the static scene for this roster.
    r.render(0, &members, 4, &mut out);

    let before = heap_ops();
    for f in 1..=120u64 {
        for (i, m) in members.iter_mut().enumerate() {
            let v = ((f * 13 + i as u64 * 7) % 100) as f32 / 100.0;
            m.level_peak = v;
            m.level_rms = v * 0.6;
        }
        r.render(f, &members, 4, &mut out);
    }
    let ops = heap_ops() - before;
    assert_eq!(ops, 0, "{ops} heap ops in steady-state render");
}
