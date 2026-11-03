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

/// Success response for a created checkpoint: its id and its size on disk.
#[must_use]
pub fn checkpoint_response(checkpoint_id: &str, bytes: u64) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Envelope<'a> {
        protocol_version: u64,
        checkpoint_id: &'a str,
        bytes: u64,
    }
    let envelope = Envelope {
        protocol_version: super::VERSION,
        checkpoint_id,
        bytes,
    };
    serde_json::to_string(&envelope).unwrap_or_default()
}
