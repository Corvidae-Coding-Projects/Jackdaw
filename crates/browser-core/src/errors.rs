use browser_protocol::{
    EffectState, ElementRef, ErrorCode, ErrorDetail, ProfileId, ProtocolError, TabId, ViewId,
};

pub(crate) fn protocol_error(code: ErrorCode, message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code,
        message: message.into(),
        retryable: false,
        effect: EffectState::None,
        details: Vec::<ErrorDetail>::new(),
    }
}

pub(crate) fn invalid_request(message: impl Into<String>) -> ProtocolError {
    protocol_error(ErrorCode::InvalidRequest, message)
}

pub(crate) fn request_id_conflict() -> ProtocolError {
    protocol_error(
        ErrorCode::RequestIdConflict,
        "request ID was already used with a different payload",
    )
}

pub(crate) fn event_gap(first_available_sequence: u64) -> ProtocolError {
    let mut error = protocol_error(
        ErrorCode::EventGap,
        format!(
            "event cursor is no longer valid; first available sequence is {first_available_sequence}"
        ),
    );
    error.details.push(ErrorDetail {
        field: Some("cursor".to_owned()),
        reason: format!("first_available_sequence={first_available_sequence}"),
    });
    error
}

pub(crate) fn internal_error(message: impl ToString) -> ProtocolError {
    protocol_error(ErrorCode::Internal, message.to_string())
}

pub(crate) fn network_error(error: impl ToString) -> ProtocolError {
    protocol_error(ErrorCode::NetworkError, error.to_string())
}

pub(crate) fn transfer_failed(message: impl Into<String>) -> ProtocolError {
    protocol_error(ErrorCode::TransferFailed, message)
}

pub(crate) fn stale_target(target: &ElementRef) -> ProtocolError {
    protocol_error(
        ErrorCode::StaleTarget,
        format!("target {target} is not present in a retained observation"),
    )
}

pub(crate) fn stale_view(view_id: &ViewId) -> ProtocolError {
    protocol_error(
        ErrorCode::StaleTarget,
        format!("observation view {view_id} is invalid for the current document"),
    )
}

pub(crate) fn view_expired(view_id: &ViewId) -> ProtocolError {
    protocol_error(
        ErrorCode::ViewExpired,
        format!("observation view {view_id} expired from retained history"),
    )
}

pub(crate) fn target_not_interactable(error: impl ToString) -> ProtocolError {
    protocol_error(ErrorCode::TargetNotInteractable, error.to_string())
}

pub(crate) fn profile_not_found(profile: &ProfileId) -> ProtocolError {
    protocol_error(
        ErrorCode::ProfileNotFound,
        format!("profile {profile} does not exist or is not open"),
    )
}

pub(crate) fn tab_not_found(tab: &TabId) -> ProtocolError {
    protocol_error(ErrorCode::TabNotFound, format!("tab {tab} does not exist"))
}
