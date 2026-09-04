#[allow(dead_code)]
#[path = "main.rs"]
mod implementation;

pub use implementation::{
    delete_direct, delete_trash, empty_trash, init, restore, scan, scan_with_control,
    set_approval, trash, trash_list,
};
