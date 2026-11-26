//! mathbuddy as a library: the binary's parts, here so the integration
//! tests (tests/) can drive real kernels and R sessions through them.

pub mod assistant;
pub mod core;
pub mod http;
pub mod kernel;
pub mod llm;
pub mod rbridge;
pub mod session;
pub mod setup;
