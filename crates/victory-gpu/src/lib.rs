//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Presentation (the
//! title's present planes → host swap chain) lives in [`queue`].

// Off Windows only the portable parts and their tests are built.
#![cfg_attr(not(windows), allow(dead_code, unused_variables, unused_mut))]

mod adapter;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(windows)]
mod compose;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
/// Diagnostics counters raised by callers and reported with frame copy statistics.
#[cfg(all(windows, feature = "diagnostics"))]
pub mod stats {
    pub use super::copystat::{C, add};
}
#[cfg(all(windows, feature = "diagnostics"))]
mod book;
#[cfg(windows)]
mod cores;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
#[cfg(all(windows, feature = "diagnostics"))]
mod faultbook;
pub mod fence;
pub mod frame;
#[cfg(any(windows, test))]
mod frame_times;
#[cfg(windows)]
mod hlsl;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
// CPU model of the GPU budget: only referenced from unit tests.
#[cfg(all(windows, feature = "diagnostics"))]
mod ab;
#[cfg(test)]
mod indirect_budget;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
mod land;
pub mod list;
#[cfg(windows)]
mod memory;
pub mod object;
#[cfg(windows)]
mod picked;
pub mod pipeline;
#[cfg(windows)]
mod pool;
#[cfg(all(windows, feature = "diagnostics"))]
mod popwatch;
#[cfg(windows)]
mod port;
#[cfg(windows)]
mod port_cbv;
#[cfg(windows)]
mod port_journal;
#[cfg(windows)]
mod primitive;
#[cfg(all(windows, feature = "diagnostics"))]
mod profile;
pub mod query;
pub mod queue;
#[cfg(windows)]
mod residency;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod rootsig;
#[cfg(windows)]
mod safecopy;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod shadow;
pub mod signature;
mod switch;
#[cfg(windows)]
mod tdr;
#[cfg(windows)]
mod texback;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
mod tiles;
pub mod timeline;
#[cfg(windows)]
pub mod titlewrite;
#[cfg(windows)]
mod upload;
#[cfg(windows)]
mod watch;

pub use frame::set_frame_interval_override;
#[cfg(windows)]
pub use host::{HdrDisplayInfo, display_hdr_info, on_failure, write_title_memory};

/// Whether `switch` is on now, if it is one of those `VICTORY_AB` turns on and
/// off while the title runs (`ab`).
#[cfg(all(windows, feature = "diagnostics"))]
pub fn switched(switch: &str) -> Option<bool> {
    ab::pick(switch)
}

/// What the frame is at (`copystat::phase`), for the sampling profiler.
#[cfg(all(windows, feature = "diagnostics"))]
pub fn frame_phase() -> u32 {
    copystat::phase()
}

/// Frames the title has begun (its frame waits that ended), for logs.
pub fn frames_begun() -> u64 {
    frame::current()
}
#[cfg(windows)]
pub use inplace::{
    title_memory_freeing, title_memory_section, title_memory_section_gone, title_memory_view,
};
#[cfg(windows)]
pub use watch::{
    KernelWrite, kernel_writing, title_memory_allocated, title_memory_freed, title_memory_reading,
    title_memory_remapped, title_memory_remapping, title_memory_splitting, title_memory_unmapping,
    title_memory_written, title_placeholder_reserved, writing as kernel_will_write,
};

/// Inspect the buffer write journal in cross-crate memory integration tests.
#[cfg(all(windows, feature = "test-support"))]
#[doc(hidden)]
pub fn buffer_writes_since(address: usize, len: usize, since: u64) -> (u64, Vec<usize>) {
    let (now, pages) =
        watch::writes(address, address.saturating_add(len), true).expect("mapped title buffer");
    (
        now,
        pages
            .into_iter()
            .filter(|&(_, time)| time > since)
            .map(|(page, _)| page * watch::PAGE)
            .collect(),
    )
}
