#![cfg(target_os = "macos")]
//! Shared Apple platform support for GPUI.
//!
//! This crate contains the Metal renderer and GPU resource management shared
//! by GPUI's Apple platform backends.

mod drawable_queue;
mod metal_atlas;
pub mod metal_renderer;
