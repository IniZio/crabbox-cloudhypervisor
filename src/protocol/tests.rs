use super::*;

#[test]
fn parses_doctor() {
    let req = parse_request(
        b"{\"protocolVersion\":1,\"operation\":\"doctor\",\"config\":{\"a\":1},\"x\":2}\n",
    )
    .unwrap();
    assert_eq!(req.operation, Operation::Doctor);
    assert_eq!(req.config.get("a"), Some(&Value::from(1)));
}

#[test]
fn absent_config_is_null() {
    let req = parse_request(br#"{"protocolVersion":1,"operation":"list"}"#).unwrap();
    assert_eq!(req.config, Value::Null);
}

#[test]
fn rejects_version_two() {
    let err = parse_request(br#"{"protocolVersion":2,"operation":"doctor"}"#).unwrap_err();
    assert_eq!(
        err,
        "unsupported protocol version 2; this adapter speaks version 1"
    );
}

#[test]
fn rejects_missing_version() {
    assert!(parse_request(br#"{"operation":"doctor"}"#).is_err());
}

#[test]
fn rejects_unknown_operation() {
    let err = parse_request(br#"{"protocolVersion":1,"operation":"foo"}"#).unwrap_err();
    assert_eq!(err, "unknown operation \"foo\"");
}

#[test]
fn rejects_missing_operation() {
    assert!(parse_request(br#"{"protocolVersion":1}"#).is_err());
}

#[test]
fn rejects_invalid_json() {
    assert!(parse_request(b"not json").is_err());
}

#[test]
fn rejects_non_object_config() {
    let err =
        parse_request(br#"{"protocolVersion":1,"operation":"doctor","config":"x"}"#).unwrap_err();
    assert!(err.contains("config"));
}

#[test]
fn errors_do_not_echo_config() {
    let err = parse_request(
        br#"{"protocolVersion":3,"operation":"doctor","config":{"apiKey":"sk_live_secret"}}"#,
    )
    .unwrap_err();
    assert!(!err.contains("sk_live_secret"));
}

#[test]
fn responses_escape_quotes() {
    assert_eq!(
        message_response("say \"hi\""),
        r#"{"protocolVersion":1,"message":"say \"hi\""}"#
    );
    assert_eq!(error_response("a\"b"), r#"{"error":"a\"b"}"#);
    assert_eq!(
        message_response("ready"),
        r#"{"protocolVersion":1,"message":"ready"}"#
    );
}

fn parse(tail: &str) -> Result<Request, String> {
    let head = r#"{"protocolVersion":1,"operation":"acquire","#;
    parse_request(format!("{head}{tail}").as_bytes())
}

#[test]
fn parses_acquire_with_desired_class_keep() {
    let d = r#""desired":{"leaseId":"l1","slug":"s","name":"n","class":"fast"}"#;
    let req = parse(&format!(r#"{d},"keep":true}}"#)).unwrap();
    assert_eq!(req.desired.unwrap().class.as_deref(), Some("fast"));
    assert!(req.keep);
}

#[test]
fn class_absent_and_desired_errors() {
    let req = parse(r#""desired":{"leaseId":"l1","slug":"s","name":"n"}}"#).unwrap();
    assert_eq!(req.desired.unwrap().class, None);
    assert!(parse(r#""desired":{"leaseId":"l1","slug":"s"}}"#).is_err());
    assert!(parse(r#""desired":"x"}"#).is_err());
}

#[test]
fn expected_empty_fields_are_none() {
    let req = parse(r#""expected":{"leaseId":"l1","slug":""}}"#).unwrap();
    let (id, slug) = req.expected.map(|e| (e.lease_id, e.slug)).unwrap();
    assert_eq!((id.as_deref(), slug), (Some("l1"), None));
    assert!(parse(r#""expected":3}"#).is_err());
    assert!(parse(r#""keep":"y"}"#).is_err());
}

#[test]
fn lease_response_shape() {
    let lease = Lease {
        lease_id: "l1".to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        cloud_id: "vm1".to_owned(),
        status: "ready".to_owned(),
        ssh: Ssh {
            user: "crabbox".to_owned(),
            host: "vm1.local".to_owned(),
            port: "22".to_owned(),
            key: "/abs/id_ed25519".to_owned(),
            proxy_command: "x proxy l1".to_owned(),
            ready_check: "true".to_owned(),
            host_key: String::new(),
        },
        growth_blocked: String::new(),
        cache_volumes: Vec::new(),
    };
    let base = r#"{"protocolVersion":1,"lease":{"leaseId":"l1","slug":"s","name":"n","cloudId":"vm1","status":"ready","ssh":{"user":"crabbox","host":"vm1.local","port":"22","key":"/abs/id_ed25519","proxyCommand":"x proxy l1","readyCheck":"true""#;
    assert_eq!(lease_response(&lease), format!("{base}}}}}}}"));
}

#[test]
fn parses_lease_ref_ignoring_other_keys() {
    let req = parse(
        r#""lease":{"leaseId":"l","slug":"s","name":"n","cloudId":"c","status":"x","labels":{}}}"#,
    )
    .unwrap();
    assert_eq!(
        req.lease,
        Some(LeaseRef {
            lease_id: Some("l".into()),
            slug: Some("s".into()),
            name: Some("n".into()),
            cloud_id: Some("c".into()),
        })
    );
}

#[test]
fn lease_absent_or_wrong_type() {
    assert_eq!(parse(r#""x":1}"#).unwrap().lease, None);
    assert!(parse(r#""lease":"l"}"#).is_err());
}

#[test]
fn parses_dry_run_and_release_only() {
    let req = parse(r#""dryRun":true,"releaseOnly":true}"#).unwrap();
    assert!(req.dry_run);
    assert!(req.release_only);
    let req = parse(r#""x":1}"#).unwrap();
    assert!(!req.dry_run);
    assert!(!req.release_only);
}

#[test]
fn rejects_non_bool_flags() {
    assert_eq!(
        parse(r#""dryRun":"y"}"#).unwrap_err(),
        "dryRun must be a boolean"
    );
    assert_eq!(
        parse(r#""releaseOnly":1}"#).unwrap_err(),
        "releaseOnly must be a boolean"
    );
}

#[test]
fn empty_leases_response_is_array() {
    assert_eq!(leases_response(&[]), r#"{"protocolVersion":1,"leases":[]}"#);
}

#[test]
fn leases_response_uses_camel_case() {
    let row = LeaseRow {
        lease_id: "l".into(),
        slug: "s".into(),
        name: "n".into(),
        cloud_id: "c".into(),
        status: "running".into(),
        growth_blocked: String::new(),
    };
    assert_eq!(
        leases_response(&[row]),
        r#"{"protocolVersion":1,"leases":[{"leaseId":"l","slug":"s","name":"n","cloudId":"c","status":"running"}]}"#
    );
}

#[test]
fn ok_response_is_bare_version() {
    assert_eq!(ok_response(), r#"{"protocolVersion":1}"#);
}

#[test]
fn parses_extra_request_fields() {
    let req = parse(
        r#""id":"l1","state":"running","reclaim":true,"force":true,"all":true,"refresh":true}"#,
    )
    .unwrap();
    assert_eq!(req.id.as_deref(), Some("l1"));
    assert_eq!(req.state.as_deref(), Some("running"));
    assert!(req.reclaim && req.force && req.all && req.refresh);
    let req = parse(r#""x":1}"#).unwrap();
    assert_eq!((req.id, req.state), (None, None));
    assert!(!req.reclaim && !req.force && !req.all && !req.refresh);
}

#[test]
fn unknown_operation_is_rejected() {
    let err = parse_request(br#"{"protocolVersion":1,"operation":"hibernate"}"#).unwrap_err();
    assert_eq!(err, "unknown operation \"hibernate\"");
}

#[test]
fn parses_pause_and_resume_with_extra_lease_fields() {
    for name in ["pause", "resume"] {
        let input = format!(
            r#"{{"protocolVersion":1,"operation":"{name}","config":{{}},"lease":{{"leaseId":"l","slug":"s","name":"n","cloudId":"c","status":"paused","serverType":"t","labels":{{"a":"b"}},"ssh":{{"host":"h"}}}}}}"#
        );
        let req = parse_request(input.as_bytes()).unwrap();
        assert_eq!(req.operation.as_str(), name);
        assert_eq!(req.lease.and_then(|l| l.lease_id).as_deref(), Some("l"));
        assert!(!req.release_only);
    }
}

#[test]
fn operation_roundtrips_through_wire_name() {
    for op in [Operation::Pause, Operation::Resume, Operation::Touch] {
        assert_eq!(Operation::from_wire(op.as_str()), Some(op));
    }
}

#[test]
fn ssh_host_key_serialized_only_when_non_empty() {
    let mut ssh = Ssh {
        user: "u".to_owned(),
        host: "h".to_owned(),
        port: "22".to_owned(),
        key: "k".to_owned(),
        proxy_command: "p".to_owned(),
        ready_check: "true".to_owned(),
        host_key: String::new(),
    };
    assert!(!serde_json::to_string(&ssh).unwrap().contains("hostKey"));
    ssh.host_key = "ssh-ed25519 AAAA".to_owned();
    assert!(serde_json::to_string(&ssh)
        .unwrap()
        .contains(r#""hostKey":"ssh-ed25519 AAAA""#));
}

#[test]
fn parses_cache_volumes_from_desired() {
    let d = r#""desired":{"leaseId":"l1","slug":"s","name":"n","cacheVolumes":[
        {"group":"7f3a","name":"gradle","key":"7f3a-gradle","paths":["~/.gradle/caches","~/.gradle/wrapper"],"sizeGB":0},
        {"group":"7f3a","name":"sdk","key":"7f3a-sdk","paths":["/opt/sdk"],"sizeGB":12,"required":false,"sealed":true}]}}"#;
    let vols = parse(d).unwrap().desired.unwrap().cache_volumes;
    assert_eq!(
        vols,
        vec![
            CacheVolumeRequest {
                group: "7f3a".into(),
                name: "gradle".into(),
                key: "7f3a-gradle".into(),
                paths: vec!["~/.gradle/caches".into(), "~/.gradle/wrapper".into()],
                size_gb: 0,
                sealed: false,
            },
            CacheVolumeRequest {
                group: "7f3a".into(),
                name: "sdk".into(),
                key: "7f3a-sdk".into(),
                paths: vec!["/opt/sdk".into()],
                size_gb: 12,
                sealed: true,
            },
        ]
    );
}

#[test]
fn cache_volumes_absent_null_or_malformed() {
    let base = r#""desired":{"leaseId":"l1","slug":"s","name":"n""#;
    let vols =
        |tail: &str| parse(&format!("{base}{tail}}}}}")).map(|r| r.desired.unwrap().cache_volumes);
    assert_eq!(vols("").unwrap(), Vec::new());
    assert_eq!(vols(r#","cacheVolumes":null"#).unwrap(), Vec::new());
    for bad in [
        r#","cacheVolumes":{}"#,
        r#","cacheVolumes":[1]"#,
        r#","cacheVolumes":[{"paths":"~/x"}]"#,
        r#","cacheVolumes":[{"paths":[1]}]"#,
        r#","cacheVolumes":[{"sizeGB":-1}]"#,
        r#","cacheVolumes":[{"sealed":"yes"}]"#,
    ] {
        assert!(vols(bad).unwrap_err().contains("cacheVolumes"), "{bad}");
    }
}

#[test]
fn lease_echoes_cache_volumes_only_when_present() {
    let mut lease = Lease {
        lease_id: "l1".to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        cloud_id: "c".to_owned(),
        status: "ready".to_owned(),
        ssh: Ssh {
            user: "u".to_owned(),
            host: "h".to_owned(),
            port: "22".to_owned(),
            key: "/k".to_owned(),
            proxy_command: "p".to_owned(),
            ready_check: "r".to_owned(),
            host_key: String::new(),
        },
        growth_blocked: String::new(),
        cache_volumes: Vec::new(),
    };
    assert!(!lease_response(&lease).contains("cacheVolumes"));
    lease.cache_volumes.push(CacheVolumeEcho {
        name: "gradle".to_owned(),
        outcome: "restored".to_owned(),
        detail: "gen 1729781203".to_owned(),
    });
    assert!(lease_response(&lease).contains(
        r#""cacheVolumes":[{"name":"gradle","outcome":"restored","detail":"gen 1729781203"}]"#
    ));
}
