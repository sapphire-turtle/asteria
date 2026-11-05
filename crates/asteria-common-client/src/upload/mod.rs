//! Uploading local files to the server's `Inbox/`.

mod engine;
mod plan;
pub mod queue;

pub(crate) use engine::EngineConfig;
pub use engine::{UploadSnapshot, Uploads, UploadsState, set_retry_backoff};
pub use plan::{DropPlan, PlanWarning, PlannedFile, PlannedGroup, plan_drop};
