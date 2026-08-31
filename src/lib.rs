#[allow(dead_code)]
#[path = "main.rs"]
mod implementation;

pub use implementation::{init, restore, scan, set_approval, trash, trash_list};
