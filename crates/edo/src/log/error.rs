//! Errors for the [`log`](super) module.

use snafu::Snafu;

/// A `status` field value that is not one of the [`TaskStatus`](super::TaskStatus) strings.
#[derive(Debug, Snafu)]
#[snafu(display("unknown task status {value:?}"))]
pub struct ParseTaskStatusError {
    value: String,
}

impl ParseTaskStatusError {
    pub(super) fn new(value: &str) -> Self {
        Self {
            value: value.to_owned(),
        }
    }
}
