use browser_protocol::{
    Command, CommandRequest, ElementRef, NavigationCommand, PROTOCOL_VERSION, ProfileId,
    ProtocolVersion, RequestId, TabId,
};

fn navigation_request() -> CommandRequest {
    CommandRequest {
        protocol_version: PROTOCOL_VERSION,
        request_id: RequestId::parse("r_contract").unwrap(),
        profile_id: ProfileId::parse("p_work").unwrap(),
        tab_id: Some(TabId::parse("t_1").unwrap()),
        command: Command::Navigate(NavigationCommand::Open {
            url: "https://example.test/path".into(),
        }),
        timeout_ms: 5_000,
    }
}

#[test]
fn request_round_trips_without_losing_typed_identity() {
    let request = navigation_request();
    let encoded = serde_json::to_string(&request).unwrap();
    let decoded: CommandRequest = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, request);
}

#[test]
fn unknown_request_fields_are_rejected() {
    let mut value = serde_json::to_value(navigation_request()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), serde_json::Value::Bool(true));
    assert!(serde_json::from_value::<CommandRequest>(value).is_err());
}

#[test]
fn unsupported_protocol_versions_are_rejected() {
    let mut value = serde_json::to_value(navigation_request()).unwrap();
    value.as_object_mut().unwrap().insert(
        "protocol_version".into(),
        serde_json::Value::from(ProtocolVersion::new(2).get()),
    );
    assert!(serde_json::from_value::<CommandRequest>(value).is_err());
}

#[test]
fn element_reference_retains_its_view_generation() {
    let target = ElementRef::parse("v_17:e_4").unwrap();
    assert_eq!(target.view_id().as_str(), "v_17");
    assert!(ElementRef::parse("e_4").is_err());
    assert!(ElementRef::parse("v_17:e_4:other").is_err());
}
