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
        // RustAllocator answers a zero-sized calloc with null, as libc may.
        // That is not a refusal, so it must not mark the budget exhausted.
        if total == 0 {
            return self.inner.calloc(count, size);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_sized_calloc_is_not_a_budget_refusal() {
        let exhausted = Rc::new(Cell::new(false));
        let mut alloc = BudgetAllocator::new(1024, exhausted.clone());
        for (count, size) in [(0, 8), (8, 0), (0, 0)] {
            assert!(alloc.calloc(count, size).is_null());
        }
        assert!(!exhausted.get());
    }

    #[test]
    fn the_cap_refuses_and_frees_return_the_accounted_bytes() {
        let exhausted = Rc::new(Cell::new(false));
        let mut alloc = BudgetAllocator::new(64, exhausted.clone());
        let first = alloc.alloc(40);
        assert!(!first.is_null() && !exhausted.get());
        assert!(alloc.alloc(40).is_null());
        assert!(exhausted.get());
        // SAFETY: `first` came from this allocator and is freed once.
        unsafe { alloc.dealloc(first) };
        assert_eq!(alloc.used, 0);
        exhausted.set(false);
        let second = alloc.alloc(40);
        assert!(!second.is_null() && !exhausted.get());
        // SAFETY: as above.
        unsafe { alloc.dealloc(second) };
    }
}
