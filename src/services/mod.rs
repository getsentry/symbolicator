//! Internal services.
//!
//! These services all operate independently from their caller. Therefore they are all created with
//! a [`ThreadPool`] to operate on so that they can spawn work without the caller having to worry
//! about how calling is going to affect it's own executor. It is common for threadpools to be
//! shared by multiple services and the application wants to generally separate services with
//! CPU-intensive workloads from those with IO-heavy workloads.
//!
//! In general, services are created once in the [`crate::app::ServiceState`] and accessed via this
//! state.
//!
//! [`ThreadPool`]: crate::utils::futures::ThreadPool

pub mod download;
