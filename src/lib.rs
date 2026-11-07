#[cfg(not(unix))]
compile_error!(
    "This release supports macOS and Linux process groups; Windows is not yet supported."
);
mod app_server;
pub mod catalog;
pub mod completion;
pub mod config;
pub mod inheritance;
pub mod process;
pub mod quota;
pub mod runner;
pub mod service;
pub mod session;
