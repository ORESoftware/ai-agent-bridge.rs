//! `/ores-claude` and `/ores-chatgpt` Slack command ingress.
//!
//! The implementation is split into textual include parts only to keep connector
//! writes reviewable. The included items share this single module scope.

include!("slack_commands_parts/part1.rs");
include!("slack_commands_parts/part2.rs");
include!("slack_commands_parts/part3.rs");
include!("slack_commands_parts/part4.rs");
include!("slack_commands_parts/part5.rs");
include!("slack_commands_parts/part6.rs");
include!("slack_commands_parts/part7.rs");
include!("slack_commands_parts/part8.rs");
include!("slack_commands_parts/part9.rs");
include!("slack_commands_parts/part10.rs");
