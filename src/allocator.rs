//! The global allocator.
//!
//! This contains primitives for the cross-thread allocator.

use prelude::*;

use core::{ops, mem, ptr};

use {brk, tls, sys};
use bookkeeper::{self, Bookkeeper, Allocator};
use sync::Mutex;

/// The global default allocator.
// TODO remove these filthy function pointers.
static GLOBAL_ALLOCATOR: Mutex<LazyInit<fn() -> GlobalAllocator, GlobalAllocator>> =
    Mutex::new(LazyInit::new(global_init));
tls! {
    /// The thread-local allocator.
    static THREAD_ALLOCATOR: MoveCell<LazyInit<fn() -> LocalAllocator, LocalAllocator>> =
        MoveCell::new(LazyInit::new(local_init));
}

/// Initialize the global allocator.
fn global_init() -> GlobalAllocator {
    // The initial acquired segment.
    let (aligner, initial_segment, excessive) =
        brk::get(bookkeeper::EXTRA_ELEMENTS * 4, mem::align_of::<Block>());

    // Initialize the new allocator.
    let mut res = GlobalAllocator {
        inner: Bookkeeper::new(unsafe {
            Vec::from_raw_parts(initial_segment, 0)
        }),
    };

    // Free the secondary space.
    res.push(aligner);
    res.push(excessive);

    res
}

/// Initialize the local allocator.
fn local_init() -> LocalAllocator {
    // The initial acquired segment.
    let initial_segment = GLOBAL_ALLOCATOR
        .lock()
        .get()
        .alloc(bookkeeper::EXTRA_ELEMENTS * 4, mem::align_of::<Block>());

    unsafe {
        // Initialize the new allocator.
        let mut res = LocalAllocator {
            inner: Bookkeeper::new(Vec::from_raw_parts(initial_segment, 0)),
        };
        // Attach the allocator to the current thread.
        res.attach();

        res
    }
}

/// Temporarily get the allocator.
///
/// This is simply to avoid repeating ourself, so we let this take care of the hairy stuff.
fn get_allocator<T, F: FnOnce(&mut LocalAllocator) -> T>(f: F) -> T {
    /// A dummy used as placeholder for the temporary initializer.
    fn dummy() -> LocalAllocator {
        unreachable!();
    }

    // Get the thread allocator.
    let thread_alloc = THREAD_ALLOCATOR.get();
    // Just dump some placeholding initializer in the place of the TLA.
    let mut thread_alloc = thread_alloc.replace(LazyInit::new(dummy));

    // Call the closure involved.
    let res = f(thread_alloc.get());

    // Put back the original allocator.
    THREAD_ALLOCATOR.get().replace(thread_alloc);

    res
}

/// Derives `Deref` and `DerefMut` to the `inner` field.
macro_rules! derive_deref {
    ($imp:ty, $target:ty) => {
        impl ops::Deref for $imp {
            type Target = $target;

            fn deref(&self) -> &$target {
                &self.inner
            }
        }

        impl ops::DerefMut for $imp {
            fn deref_mut(&mut self) -> &mut $target {
                &mut self.inner
            }
        }
    };
}

/// Global SBRK-based allocator.
///
/// This will extend the data segment whenever new memory is needed. Since this includes leaving
/// userspace, this shouldn't be used when other allocators are available (i.e. the bookkeeper is
/// local).
struct GlobalAllocator {
    // The inner bookkeeper.
    inner: Bookkeeper,
}

derive_deref!(GlobalAllocator, Bookkeeper);

impl Allocator for GlobalAllocator {
    #[inline]
    fn alloc_fresh(&mut self, size: usize, align: usize) -> Block {
        // Obtain what you need.
        let (alignment_block, res, excessive) = brk::get(size, align);

        // Add it to the list. This will not change the order, since the pointer is higher than all
        // the previous blocks (BRK extends the data segment). Although, it is worth noting that
        // the stack is higher than the program break.
        self.push(alignment_block);
        self.push(excessive);

        res
    }
}

/// A local allocator.
///
/// This acquires memory from the upstream (global) allocator, which is protected by a `Mutex`.
pub struct LocalAllocator {
    // The inner bookkeeper.
    inner: Bookkeeper,
}

derive_deref!(LocalAllocator, Bookkeeper);

impl LocalAllocator {
    /// Attach this allocator to the current thread.
    ///
    /// This will make sure this allocator's data  is freed to the
    pub unsafe fn attach(&mut self) {
        extern fn dtor(ptr: *mut LocalAllocator) {
            let alloc = unsafe { ptr::read(ptr) };

            // Lock the global allocator.
            // TODO dumb borrowck
            let mut global_alloc = GLOBAL_ALLOCATOR.lock();
            let global_alloc = global_alloc.get();

            // Gotta' make sure no memleaks are here.
            #[cfg(feature = "debug_tools")]
            alloc.assert_no_leak();

            // TODO, we know this is sorted, so we could abuse that fact to faster insertion in the
            // global allocator.

            alloc.inner.for_each(move |block| global_alloc.free(block));
        }

        sys::register_thread_destructor(self as *mut LocalAllocator, dtor).unwrap();
    }
}

impl Allocator for LocalAllocator {
    #[inline]
    fn alloc_fresh(&mut self, size: usize, align: usize) -> Block {
        /// Canonicalize the requested space.
        ///
        /// We request excessive space to the upstream allocator to avoid repeated requests and
        /// lock contentions.
        #[inline]
        fn canonicalize_space(min: usize) -> usize {
            // TODO tweak this.

            // To avoid having mega-allocations allocate way to much space, we
            // have a maximal extra space limit.
            if min > 8192 { min } else {
                // To avoid paying for short-living or little-allocating threads, we have no minimum.
                // Instead we multiply.
                min * 4
                // This won't overflow due to the conditition of this branch.
            }
        }

        // Get the block from the global allocator.
        let (res, excessive) = GLOBAL_ALLOCATOR.lock()
            .get()
            .alloc(canonicalize_space(size), align)
            .split(size);

        // Free the excessive space to the current allocator. Note that you cannot simply push
        // (which is the case for SBRK), due the block not necessarily being above all the other
        // blocks in the pool. For this reason, we let `free` handle the search and so on.
        self.free(excessive);

        res
    }
}

/// Allocate a block of memory.
///
/// # Errors
///
/// The OOM handler handles out-of-memory conditions.
#[inline]
pub fn alloc(size: usize, align: usize) -> *mut u8 {
    get_allocator(|alloc| {
        *Pointer::from(alloc.alloc(size, align))
    })
}

/// Free a buffer.
///
/// Note that this do not have to be a buffer allocated through ralloc. The only requirement is
/// that it is not used after the free.
///
/// # Important!
///
/// You should only allocate buffers allocated through `ralloc`. Anything else is considered
/// invalid.
///
/// # Errors
///
/// The OOM handler handles out-of-memory conditions.
///
/// # Safety
///
/// Rust assume that the allocation symbols returns correct values. For this reason, freeing
/// invalid pointers might introduce memory unsafety.
///
/// Secondly, freeing an used buffer can introduce use-after-free.
#[inline]
pub unsafe fn free(ptr: *mut u8, size: usize) {
    get_allocator(|alloc| {
        alloc.free(Block::from_raw_parts(Pointer::new(ptr), size))
    });
}

/// Reallocate memory.
///
/// Reallocate the buffer starting at `ptr` with size `old_size`, to a buffer starting at the
/// returned pointer with size `size`.
///
/// # Important!
///
/// You should only reallocate buffers allocated through `ralloc`. Anything else is considered
/// invalid.
///
/// # Errors
///
/// The OOM handler handles out-of-memory conditions.
///
/// # Safety
///
/// Due to being able to potentially memcpy an arbitrary buffer, as well as shrinking a buffer,
/// this is marked unsafe.
#[inline]
pub unsafe fn realloc(ptr: *mut u8, old_size: usize, size: usize, align: usize) -> *mut u8 {
    get_allocator(|alloc| {
        *Pointer::from(alloc.realloc(
            Block::from_raw_parts(Pointer::new(ptr), old_size),
            size,
            align
        ))
    })
}

/// Try to reallocate the buffer _inplace_.
///
/// In case of success, return the new buffer's size. On failure, return the old size.
///
/// This can be used to shrink (truncate) a buffer as well.
///
/// # Safety
///
/// Due to being able to shrink (and thus free) the buffer, this is marked unsafe.
#[inline]
pub unsafe fn realloc_inplace(ptr: *mut u8, old_size: usize, size: usize) -> Result<(), ()> {
    get_allocator(|alloc| {
        if alloc.realloc_inplace(
            Block::from_raw_parts(Pointer::new(ptr), old_size),
            size
        ).is_ok() {
            Ok(())
        } else {
            Err(())
        }
    })
}
