//! Stable-Cargo feature selections shared by native workspace libraries.
//!
//! This crate has no runtime interface. Its dependency declarations prevent
//! switching build/workspace/package commands from rebuilding shared libraries.
#![no_std]
