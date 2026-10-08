//! Allocation accounting with a native refusal signal, independent of error text.
use rquickjs::allocator::{Allocator, RustAllocator};
use std::{cell::Cell, rc::Rc};

pub(super) struct BudgetAllocator {
    inner: RustAllocator,
    used: usize,
    cap: usize,
    exhausted: Rc<Cell<bool>>,
}

impl BudgetAllocator {
    pub(super) fn new(cap: usize, exhausted: Rc<Cell<bool>>) -> Self {
        Self {
            inner: RustAllocator,
            used: 0,
            cap,
            exhausted,
        }
    }

    fn permits(&self, size: usize, old: usize) -> bool {
        // RustAllocator rounds to u64 alignment and adds a usize header.
        let cost = size
            .checked_add(7)
            .map(|n| n & !7)
            .and_then(|n| n.checked_add(8));
        let permitted = cost
            .and_then(|n| (self.used - old).checked_add(n))
            .is_some_and(|n| n <= self.cap);
        if !permitted {
            self.exhausted.set(true);
        }
        permitted
    }

    fn allocated(&mut self, ptr: *mut u8) -> *mut u8 {
        if ptr.is_null() {
            self.exhausted.set(true);
        } else {
            // SAFETY: every successful allocation belongs to RustAllocator.
            self.used += unsafe { RustAllocator::usable_size(ptr) } + 8;
        }
        ptr
    }
}

// SAFETY: allocation, alignment and pointer ownership are delegated unchanged
// to RustAllocator. Accounting refuses oversized requests before delegation.
unsafe impl Allocator for BudgetAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.permits(size, 0) {
            return std::ptr::null_mut();
        }
        let ptr = self.inner.alloc(size);
        self.allocated(ptr)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(total) = count.checked_mul(size) else {
            self.exhausted.set(true);
            return std::ptr::null_mut();
        };
        if !self.permits(total, 0) {
            return std::ptr::null_mut();
        }
        let ptr = self.inner.calloc(count, size);
        self.allocated(ptr)
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        // SAFETY: the engine returns only pointers allocated by this allocator.
        unsafe {
            self.used -= RustAllocator::usable_size(ptr) + 8;
            self.inner.dealloc(ptr);
        }
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(size);
        }
        if size == 0 {
            // SAFETY: same ownership guarantee as dealloc.
            unsafe {
                self.dealloc(ptr);
            }
            return std::ptr::null_mut();
        }
        // SAFETY: ptr belongs to RustAllocator until a successful realloc.
        let old = unsafe { RustAllocator::usable_size(ptr) } + 8;
        if !self.permits(size, old) {
            return std::ptr::null_mut();
        }
        // SAFETY: the original pointer is valid and the bounded size is nonzero.
        let new = unsafe { self.inner.realloc(ptr, size) };
        if !new.is_null() {
            self.used -= old;
        }
        self.allocated(new)
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize {
        // SAFETY: the engine supplies a live pointer allocated by RustAllocator.
        unsafe { RustAllocator::usable_size(ptr) }
    }
}
