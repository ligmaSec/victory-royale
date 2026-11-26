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
//! share, [`console`] which console model the title sees. Presentation
//! (the title's present plane → host swap chain) lives in [`queue`].

// Off Windows there is no host GPU: only the portable parts are built (for
// their tests), and the paths that feed the host go unused.
#![cfg_attr(not(windows), allow(dead_code, unused_variables, unused_mut))]

mod adapter;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
/// Diagnostics: counters raised by this crate's callers (the title's page
/// scatters), which ride the frame line with the copy statistics.
#[cfg(all(windows, feature = "diagnostics"))]
pub mod stats {
    pub use super::copystat::{C, add};
}

pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
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
#[cfg(test)]
mod indirect_budget;
pub mod list;
pub mod object;
#[cfg(windows)]
mod packed_copy;
pub mod pipeline;
#[cfg(windows)]
mod primitive;
#[cfg(all(windows, feature = "diagnostics"))]
mod profile;
pub mod query;
pub mod queue;
#[cfg(windows)]
mod rectangles;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod shadow;
pub mod signature;
mod switch;
#[cfg(windows)]
mod texture;
mod timeline;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
pub use host::{HdrDisplayInfo, display_hdr_info, on_failure, write_title_memory};
#[cfg(windows)]
pub use watch::{kernel_will_write, kernel_wrote, title_memory_remapped};
