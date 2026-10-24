//! Bounded native-auth diagnostics. Never accept credential or configuration text.
//! Generation zero denotes operations without an associated credential lease.

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Restore,
    Refresh,
    Load,
    Save,
    Delete,
    Bearer,
    Result,
    Credential,
}

#[derive(Clone, Copy)]
pub(super) enum Outcome {
    Started,
    Success,
    Hit,
    Miss,
    Unavailable,
    Malformed,
    InvalidGrant,
    Transport,
    Discovery,
    Other,
    Stale,
    Rejected,
}

pub(super) fn emit(stage: Stage, outcome: Outcome, generation: u64) {
    #[cfg(feature = "native-tracing")]
    {
        let stage = match stage {
            Stage::Restore => "restore",
            Stage::Refresh => "refresh",
            Stage::Load => "store_load",
            Stage::Save => "store_save",
            Stage::Delete => "store_delete",
            Stage::Bearer => "bearer_install",
            Stage::Result => "result",
            Stage::Credential => "credential",
        };
        let category = match outcome {
            Outcome::Started => "started",
            Outcome::Success => "success",
            Outcome::Hit => "hit",
            Outcome::Miss => "miss",
            Outcome::Unavailable => "unavailable",
            Outcome::Malformed => "malformed",
            Outcome::InvalidGrant => "invalid_grant",
            Outcome::Transport => "transport",
            Outcome::Discovery => "discovery",
            Outcome::Other => "other",
            Outcome::Stale => "stale",
            Outcome::Rejected => "rejected",
        };
        if matches!(
            outcome,
            Outcome::Started | Outcome::Success | Outcome::Hit | Outcome::Miss
        ) {
            tracing::info!(target: "waltr_client::native_access::telemetry", event = "native.auth", stage, category, generation);
        } else {
            tracing::warn!(target: "waltr_client::native_access::telemetry", event = "native.auth", stage, category, generation);
        }
    }
    #[cfg(not(feature = "native-tracing"))]
    let _ = (stage, outcome, generation);
}

pub(super) fn failure(error: &super::NativeAuthError) -> Outcome {
    if error.is_invalid_grant() {
        Outcome::InvalidGrant
    } else if error.0 == "token refresh request failed" {
        Outcome::Transport
    } else {
        Outcome::Other
    }
}
