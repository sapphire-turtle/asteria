//! Uploading local files to the server's `Inbox/`.

mod plan;
pub mod queue;

pub use plan::{DropPlan, PlanWarning, PlannedFile, PlannedGroup, plan_drop};
