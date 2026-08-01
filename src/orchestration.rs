//! First-class workflow coordination layered over the existing conversation bus.
//!
//! This module deliberately does not call model-vendor APIs. Provider adapters
//! register as ordinary bridge agents and execute assignments delivered through
//! the existing channel context plus REST/SSE/TCP messaging surfaces. Repository
//! edits remain guarded by the existing Fiducia-backed fenced file-lease API.

include!("orchestration/part1.rs");
include!("orchestration/part2.rs");
include!("orchestration/part3.rs");
