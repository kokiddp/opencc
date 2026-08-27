//! opencc — runs Claude Code against alternative backends (OpenAI, OpenCode)
//! through a local proxy. Shared code between the `opencc` wrapper binary and
//! the `opencc-proxy` server binary.

pub mod effort;
pub mod menus;
pub mod models;
pub mod picker;
pub mod proxy;
pub mod state;
pub mod util;
