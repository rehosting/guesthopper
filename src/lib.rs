//! guesthopper: the in-guest command channel agent.
//!
//! The framing codec and session logic live in the library crate so they can
//! be driven by integration tests over an in-memory duplex (no vsock needed).
//! The binary (`main.rs`) wires these onto the real vsock transport.

pub mod frame;
pub mod session;
