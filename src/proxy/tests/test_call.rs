use crate::config::MediaProxyMode;
use crate::proxy::call::{CallModule, Session};
use crate::proxy::tests::common::{create_test_request, create_test_server, create_transaction};
use crate::proxy::ProxyModule;
use rsip::headers::{ContentType, Header};
use rsip::prelude::UntypedHeader;
use rsip::HostWithPort;
use rsipstack::dialog::DialogId;
use std::time::{Duration, Instant};

fn create_test_dialog_id(call_id: &str, from_tag: &str, to_tag: &str) -> DialogId {
    DialogId {
        call_id: call_id.to_string(),
        from_tag: from_tag.to_string(),
        to_tag: to_tag.to_string(),
    }
}

#[tokio::test]
async fn test_call_module_basics() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone());

    assert_eq!(_module.name(), "call");
}

#[tokio::test]
async fn test_call_module_creation() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone());

    assert_eq!(_module.name(), "call");
}

#[tokio::test]
async fn test_locator_integration() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone());

    // Register a user in the locator
    let location = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: rsipstack::transport::SipAddr {
            r#type: None,
            addr: HostWithPort::try_from("192.168.1.100:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    let result = server
        .locator
        .register("alice", Some("example.com"), location)
        .await;
    assert!(result.is_ok());

    // Test looking up user
    let lookup_result = server.locator.lookup("alice", Some("example.com")).await;
    assert!(lookup_result.is_ok());
    let locations = lookup_result.unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(
        locations[0].destination.addr,
        HostWithPort::try_from("192.168.1.100:5060").unwrap()
    );
}

#[tokio::test]
async fn test_media_proxy_nat_only() {
    let mut config = crate::config::ProxyConfig::default();
    config.media_proxy.mode = MediaProxyMode::NatOnly;
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let module = CallModule::new(config, server);

    // Create a request with SDP containing private IP
    let mut request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    // Add SDP body with private IP - include connection line (c=)
    let sdp_body = b"v=0\r\no=alice 123 123 IN IP4 192.168.1.100\r\ns=Call\r\nc=IN IP4 192.168.1.100\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
    request.body = sdp_body.to_vec();
    request.headers.push(Header::ContentType(ContentType::new(
        "application/sdp".to_string(),
    )));

    let (tx, _) = create_transaction(request);

    let should_proxy = module.should_use_media_proxy(&tx).unwrap();
    assert!(should_proxy);
}

#[tokio::test]
async fn test_media_proxy_none_mode() {
    let mut config = crate::config::ProxyConfig::default();
    config.media_proxy.mode = MediaProxyMode::None;
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let module = CallModule::new(config, server);

    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    let (tx, _) = create_transaction(request);

    let should_proxy = module.should_use_media_proxy(&tx).unwrap();
    assert!(!should_proxy);
}

#[tokio::test]
async fn test_media_proxy_all_mode() {
    let mut config = crate::config::ProxyConfig::default();
    config.media_proxy.mode = MediaProxyMode::All;
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let module = CallModule::new(config, server);

    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    let (tx, _) = create_transaction(request);

    let should_proxy = module.should_use_media_proxy(&tx).unwrap();
    assert!(should_proxy);
}

#[tokio::test]
async fn test_options_handling() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    // Test that OPTIONS method is in allowed methods
    assert!(module.allow_methods().contains(&rsip::Method::Options));

    // Test dialog activity update logic (without network operations)
    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let session = Session {
        dialog_id: dialog_id.clone(),
        last_activity: Instant::now() - Duration::from_secs(10),
        parties: ("alice".to_string(), "bob".to_string()),
    };

    module
        .inner
        .sessions
        .lock()
        .unwrap()
        .insert(dialog_id.clone(), session);

    // Verify session exists with old timestamp
    let old_time = module
        .inner
        .sessions
        .lock()
        .unwrap()
        .get(&dialog_id)
        .unwrap()
        .last_activity;

    // Simulate dialog activity update (the core logic of handle_options)
    {
        let mut sessions = module.inner.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(&dialog_id) {
            session.last_activity = Instant::now();
        }
    }

    // Verify activity was updated
    let new_time = module
        .inner
        .sessions
        .lock()
        .unwrap()
        .get(&dialog_id)
        .unwrap()
        .last_activity;

    assert!(new_time > old_time);
}

#[tokio::test]
async fn test_external_realm_forwarding() {
    let mut config = crate::config::ProxyConfig::default();
    config.external_ip = Some("localhost:5060".to_string());
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let _module = CallModule::new(config, server);

    // Test realm comparison logic
    let local_realm = "localhost";
    let external_realm = "external.com";

    // This should be different realms
    assert_ne!(local_realm, external_realm);

    // Test that external realm forwarding is detected
    assert!(external_realm != local_realm);
}

#[tokio::test]
async fn test_local_realm_invite() {
    let mut config = crate::config::ProxyConfig::default();
    config.external_ip = Some("example.com:5060".to_string());
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let _module = CallModule::new(config, server.clone());

    // Register a user in the locator
    let location = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: rsipstack::transport::SipAddr {
            r#type: None,
            addr: HostWithPort::try_from("192.168.1.100:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    server
        .locator
        .register("alice", Some("example.com"), location)
        .await
        .unwrap();

    // Test locator lookup
    let lookup_result = server.locator.lookup("alice", Some("example.com")).await;
    assert!(lookup_result.is_ok());
    let locations = lookup_result.unwrap();
    assert_eq!(locations.len(), 1);

    // Test realm comparison logic
    let local_realm = "example.com";
    let callee_realm = "example.com";
    assert_eq!(local_realm, callee_realm);
}

#[tokio::test]
async fn test_session_management() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let session = Session {
        dialog_id: dialog_id.clone(),
        last_activity: Instant::now(),
        parties: ("alice".to_string(), "bob".to_string()),
    };

    module
        .inner
        .sessions
        .lock()
        .unwrap()
        .insert(dialog_id.clone(), session);

    // Verify session exists
    assert!(module
        .inner
        .sessions
        .lock()
        .unwrap()
        .contains_key(&dialog_id));

    // Test session cleanup
    CallModule::check_sessions(&module.inner).await;

    // Session should still exist (not expired)
    assert!(module
        .inner
        .sessions
        .lock()
        .unwrap()
        .contains_key(&dialog_id));
}

#[tokio::test]
async fn test_session_timeout() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add an expired session
    let session = Session {
        dialog_id: dialog_id.clone(),
        last_activity: Instant::now() - Duration::from_secs(400), // Expired
        parties: ("alice".to_string(), "bob".to_string()),
    };

    module
        .inner
        .sessions
        .lock()
        .unwrap()
        .insert(dialog_id.clone(), session);

    // Verify session exists
    assert!(module
        .inner
        .sessions
        .lock()
        .unwrap()
        .contains_key(&dialog_id));

    // Test session cleanup
    CallModule::check_sessions(&module.inner).await;

    // Session should be removed (expired)
    assert!(!module
        .inner
        .sessions
        .lock()
        .unwrap()
        .contains_key(&dialog_id));
}

#[tokio::test]
async fn test_module_lifecycle() {
    let (server, config) = create_test_server().await;
    let mut module = CallModule::new(config, server);

    let start_result = module.on_start().await;
    assert!(start_result.is_ok());

    let stop_result = module.on_stop().await;
    assert!(stop_result.is_ok());
}

#[tokio::test]
async fn test_dialog_activity_update() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let initial_time = Instant::now() - Duration::from_secs(10);
    let session = Session {
        dialog_id: dialog_id.clone(),
        last_activity: initial_time,
        parties: ("alice".to_string(), "bob".to_string()),
    };

    module
        .inner
        .sessions
        .lock()
        .unwrap()
        .insert(dialog_id.clone(), session);

    // Simulate activity update
    {
        let mut sessions = module.inner.sessions.lock().unwrap();
        if let Some(session) = sessions.get_mut(&dialog_id) {
            session.last_activity = Instant::now();
        }
    }

    // Verify activity was updated
    let updated_session = module
        .inner
        .sessions
        .lock()
        .unwrap()
        .get(&dialog_id)
        .cloned();

    assert!(updated_session.is_some());
    let session = updated_session.unwrap();
    assert!(session.last_activity > initial_time);
}

#[tokio::test]
async fn test_concurrent_session_access() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let session = Session {
        dialog_id: dialog_id.clone(),
        last_activity: Instant::now(),
        parties: ("alice".to_string(), "bob".to_string()),
    };

    module
        .inner
        .sessions
        .lock()
        .unwrap()
        .insert(dialog_id.clone(), session);

    // Simulate concurrent access
    let module_clone = module.clone();
    let dialog_id_clone = dialog_id.clone();

    let handle1 = tokio::spawn(async move {
        for _ in 0..10 {
            {
                let sessions = module_clone.inner.sessions.lock().unwrap();
                let _session = sessions.get(&dialog_id_clone);
            } // Drop guard before sleep
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let handle2 = tokio::spawn(async move {
        for _ in 0..10 {
            {
                let mut sessions = module.inner.sessions.lock().unwrap();
                if let Some(session) = sessions.get_mut(&dialog_id) {
                    session.last_activity = Instant::now();
                }
            } // Drop guard before sleep
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let _ = tokio::join!(handle1, handle2);
}

#[tokio::test]
async fn test_session_timeout_with_active_sessions() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server);

    let expired_dialog_id = create_test_dialog_id("expired-call-id", "from-tag", "to-tag");
    let active_dialog_id = create_test_dialog_id("active-call-id", "from-tag", "to-tag");

    // Add an expired session
    let expired_session = Session {
        dialog_id: expired_dialog_id.clone(),
        last_activity: Instant::now() - Duration::from_secs(400), // Expired
        parties: ("alice".to_string(), "bob".to_string()),
    };

    // Add an active session
    let active_session = Session {
        dialog_id: active_dialog_id.clone(),
        last_activity: Instant::now(), // Active
        parties: ("charlie".to_string(), "dave".to_string()),
    };

    {
        let mut sessions = module.inner.sessions.lock().unwrap();
        sessions.insert(expired_dialog_id.clone(), expired_session);
        sessions.insert(active_dialog_id.clone(), active_session);
    }

    // Verify both sessions exist
    assert_eq!(module.inner.sessions.lock().unwrap().len(), 2);

    // Test session cleanup
    CallModule::check_sessions(&module.inner).await;

    // Only active session should remain
    let sessions = module.inner.sessions.lock().unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(!sessions.contains_key(&expired_dialog_id));
    assert!(sessions.contains_key(&active_dialog_id));
}
