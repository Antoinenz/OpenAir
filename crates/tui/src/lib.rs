//! Terminal UI for OpenAir: an interactive device picker and a live streaming
//! dashboard.
//!
//! This is a library, not a binary — `openair` is the single binary and it
//! drives these screens directly. See
//! `docs/superpowers/specs/2026-08-18-tui-design.md`.
//!
//! Deliberately does **not** depend on `openair-capture`: platform-specific
//! concerns (`--handoff`, Windows now-playing) stay behind the CLI's `cfg`
//! boundaries and reach the TUI as plain data.

pub mod dashboard;
pub mod dashboard_ui;
pub mod logs;
pub mod picker;
pub mod picker_ui;
pub mod settings;
pub mod term;

pub use dashboard_ui::{spawn_dashboard, DashboardHandle, Summary};
pub use logs::{LogBuffer, LogLayer, LogLine};
pub use picker::{PickerAction, PickerRow, PickerState};
pub use picker_ui::{run_picker, PickerOutcome};
pub use term::is_interactive;
pub use settings::{GraphKind, Overrides, Settings};
