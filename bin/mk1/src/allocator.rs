//! Custom allocator implementation.
//!
//! This implementation is based on the allocator from the Reth project:
//! <https://github.com/paradigmxyz/reth/blob/main/crates/cli/util/src/allocator.rs>
//!
//! We provide support for jemalloc on unix systems.

#[cfg(all(feature = "jemalloc", unix))]
type AllocatorInner = tikv_jemallocator::Jemalloc;
#[cfg(not(all(feature = "jemalloc", unix)))]
type AllocatorInner = std::alloc::System;

/// Custom allocator.
pub(crate) type Allocator = AllocatorInner;

/// Creates a new [custom allocator][Allocator].
pub(crate) const fn new_allocator() -> Allocator {
    AllocatorInner {}
}
