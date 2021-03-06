//! Reputation engine

#![deny(rust_2018_idioms)]
#![deny(non_upper_case_globals)]
#![deny(non_camel_case_types)]
#![deny(non_snake_case)]
#![deny(unused_mut)]
#![deny(missing_docs)]

pub mod trs;
pub use trs::TotalReputationSet;

pub mod ars;
pub use ars::ActiveReputationSet;
