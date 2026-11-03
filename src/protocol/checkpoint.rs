//! Wire pieces of the checkpoint operations.

use serde::Serialize;
use serde_json::Value;

use super::opt_str;

/// Longest checkpoint id.
const MAX_CHECKPOINT_ID_LEN: usize = 64;

/// The checkpoint id of a checkpoint operation: 1..=64 chars of `[A-Za-z0-9_-]`,
/// so it is safe as a directory name.
pub(super) fn parse_checkpoint_id(
    object: &serde_json::Map<String, Value>,
) -> Result<String, String> {
    let id = opt_str(object, "checkpointId")
        .ok_or_else(|| "checkpointId must be a non-empty string".to_owned())?;
    let ok = id.len() <= MAX_CHECKPOINT_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(id)
    } else {
        Err(format!(
            "checkpointId must be 1-{MAX_CHECKPOINT_ID_LEN} characters of A-Z a-z 0-9 _ -"
        ))
    }
}

/// A checkpoint just written, as the create response reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointMade {
    /// Its id.
    pub checkpoint_id: String,
    /// The checkpoint its disk is a delta over; absent for the first of a chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_checkpoint_id: Option<String>,
    /// Bytes on disk only it holds, which deleting it frees.
    pub unique_bytes: u64,
    /// Sum of its files' lengths, holes included.
    pub apparent_bytes: u64,
}

/// Success response for a created checkpoint. `bytes` repeats `uniqueBytes`,
/// the size Crabbox records.
#[must_use]
pub fn checkpoint_response(made: &CheckpointMade) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Envelope<'a> {
        protocol_version: u64,
        bytes: u64,
        #[serde(flatten)]
        made: &'a CheckpointMade,
    }
    let envelope = Envelope {
        protocol_version: super::VERSION,
        bytes: made.unique_bytes,
        made,
    };
    serde_json::to_string(&envelope).unwrap_or_default()
}
