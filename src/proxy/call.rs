use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::config::{MediaProxyMode, ProxyConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::header_pop;
use rsipstack::rsip_ext::RsipHeadersExt;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::select;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
pub(crate) struct Session {
    pub dialog_id: DialogId,
    pub last_activity: Instant,
    pub parties: (String, String), // (caller, callee)
}

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    server: SipServerRef,
    pub(crate) sessions: Arc<Mutex<HashMap<DialogId, Session>>>,
    session_timeout: Duration,
    options_interval: Duration,
}

#[derive(Clone)]
pub struct CallModule {
    pub(crate) inner: Arc<CallModuleInner>,
}

impl CallModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CallModule::new(config, server);
        Ok(Box::new(module))
    }

    pub fn new(config: Arc<ProxyConfig>, server: SipServerRef) -> Self {
        let session_timeout = Duration::from_secs(300);
        let options_interval = Duration::from_secs(30);

        let inner = Arc::new(CallModuleInner {
            config,
            server: server.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            session_timeout,
            options_interval,
        });

        let module = Self { inner };
        module.start_session_monitor(server);
        module
    }

    fn start_session_monitor(&self, server: SipServerRef) {
        let inner = self.inner.clone();
        let endpoint = server.cancel_token.clone();

        tokio::spawn(async move {
            let mut interval = interval(inner.options_interval);
            loop {
                tokio::select! {
                    _ = endpoint.cancelled() => {
                        info!("Call module session monitor shutting down");
                        break;
                    },
                    _ = interval.tick() => {
                        Self::check_sessions(&inner).await;
                    }
                }
            }
        });
    }

    pub(crate) async fn check_sessions(inner: &CallModuleInner) {
        let now = Instant::now();
        let mut expired_sessions = Vec::new();

        {
            let sessions = inner.sessions.lock().unwrap();
            for (dialog_id, session) in sessions.iter() {
                if now.duration_since(session.last_activity) > inner.session_timeout {
                    expired_sessions.push(dialog_id.clone());
                }
            }
        }

        for dialog_id in expired_sessions {
            info!("Session timeout for dialog: {}", dialog_id);
            inner.sessions.lock().unwrap().remove(&dialog_id);
        }
    }

    /// Check if media proxy is needed based on nat_only configuration
    pub(crate) fn should_use_media_proxy(&self, tx: &Transaction) -> Result<bool> {
        let media_config = &self.inner.config.media_proxy;

        match media_config.mode {
            MediaProxyMode::None => Ok(false),
            MediaProxyMode::All => Ok(true),
            MediaProxyMode::NatOnly => {
                if let Some(content_type) = tx.original.headers.iter().find_map(|h| match h {
                    rsip::Header::ContentType(ct) => Some(ct),
                    _ => None,
                }) {
                    if content_type.value().contains("application/sdp") {
                        let body = String::from_utf8_lossy(&tx.original.body);
                        return Ok(crate::net_tool::sdp_contains_private_ip(&body).unwrap_or(false));
                    }
                }
                Ok(false)
            }
        }
    }

    /// Forward request to external proxy realm
    async fn forward_to_proxy(&self, tx: &mut Transaction, target_realm: &str) -> Result<()> {
        if !self.inner.config.enable_forwarding.unwrap_or(false) {
            return Err(anyhow!("External proxy forwarding is disabled"));
        }

        warn!(
            key = ?tx.key,
            "External proxy forwarding not implemented for realm: {}", target_realm
        );
        return Err(anyhow!("External proxy forwarding not implemented"));
    }

    async fn handle_invite(&self, tx: &mut Transaction) -> Result<()> {
        let caller = tx.original.from_header()?.uri()?.to_string();
        let callee_uri = tx.original.to_header()?.uri()?;
        let callee = callee_uri.user().unwrap_or_default().to_string();
        let callee_realm = callee_uri.host().to_string();

        if !self.inner.config.is_same_realm(&callee_realm) {
            info!(callee_realm, "Forwarding INVITE to external realm");
            return self.forward_to_proxy(tx, &callee_realm).await;
        }

        let target_locations = match self
            .inner
            .server
            .locator
            .lookup(&callee, Some(&callee_realm))
            .await
        {
            Ok(locations) => locations,
            Err(_) => {
                info!("User not found in locator: {}@{}", callee, callee_realm);
                tx.reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e))?;
                while let Some(msg) = tx.receive().await {
                    match msg {
                        rsip::message::SipMessage::Request(req) => match req.method {
                            rsip::Method::Ack => {
                                debug!("Received ACK for 404 Not Found");
                                break;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                return Ok(());
            }
        };

        let target_location = &target_locations[0];

        let should_proxy_media = self.should_use_media_proxy(tx)?;
        if should_proxy_media {
            info!("Media proxy required for NAT traversal");
        }

        let mut inv_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        inv_req.headers.push_front(via.into());

        if let Ok(record_route) = tx.endpoint_inner.get_record_route() {
            inv_req.headers.push_front(record_route.into());
        }

        let key = TransactionKey::from_request(&inv_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;
        info!(
            "Forwarding INVITE: {} -> {}",
            caller, target_location.destination
        );

        let mut inv_tx = Transaction::new_client(key, inv_req, tx.endpoint_inner.clone(), None);
        inv_tx.destination = Some(target_location.destination.clone());
        inv_tx.send().await.map_err(|e| anyhow!(e))?;

        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to create dialog ID: {}", e);
                return tx
                    .reply(rsip::StatusCode::ServerInternalError)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        loop {
            if inv_tx.is_terminated() {
                break;
            }

            select! {
                msg = inv_tx.receive() => {
                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Response(mut resp) => {
                                header_pop!(resp.headers, rsip::Header::Via);
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    let session = Session {
                                        dialog_id: dialog_id.clone(),
                                        last_activity: Instant::now(),
                                        parties: (caller.clone(), callee.clone()),
                                    };
                                    self.inner.sessions.lock().unwrap().insert(dialog_id.clone(), session);
                                    info!("Session established: {}", dialog_id);
                                }
                                tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                            }
                            _ => {}
                        }
                    }
                }
                msg = tx.receive() => {
                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Request(req) => match req.method {
                                rsip::Method::Ack => {
                                    let mut ack_req = req.clone();
                                    let via = tx.endpoint_inner.get_via(None, None).map_err(|e| anyhow!(e))?;
                                    ack_req.headers.push_front(via.into());
                                    let key = TransactionKey::from_request(&ack_req, TransactionRole::Client).map_err(|e| anyhow!(e))?;
                                    let mut ack_tx = Transaction::new_client(key, ack_req, tx.endpoint_inner.clone(), None);
                                    ack_tx.destination = Some(target_location.destination.clone());
                                    ack_tx.send().await.map_err(|e| anyhow!(e))?;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_bye(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to parse dialog ID: {}", e);
                return tx
                    .reply(rsip::StatusCode::BadRequest)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let session = {
            let sessions = self.inner.sessions.lock().unwrap();
            sessions.get(&dialog_id).cloned()
        };

        let (_, callee) = match session {
            Some(s) => s.parties,
            None => {
                info!("Session not found for BYE: {}", dialog_id);
                return tx
                    .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let target_locations = match self.inner.server.locator.lookup(&callee, None).await {
            Ok(locations) => locations,
            Err(_) => {
                info!("Target user not found for BYE: {}", callee);
                return tx
                    .reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let target_location = &target_locations[0];
        let mut bye_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        bye_req.headers.push_front(via.into());

        let key = TransactionKey::from_request(&bye_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;
        let mut bye_tx = Transaction::new_client(key, bye_req, tx.endpoint_inner.clone(), None);
        bye_tx.destination = Some(target_location.destination.clone());
        bye_tx.send().await.map_err(|e| anyhow!(e))?;

        while let Some(msg) = bye_tx.receive().await {
            match msg {
                rsip::message::SipMessage::Response(mut resp) => {
                    header_pop!(resp.headers, rsip::Header::Via);
                    tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                    break;
                }
                _ => {}
            }
        }

        self.inner.sessions.lock().unwrap().remove(&dialog_id);
        info!("Session terminated: {}", dialog_id);
        Ok(())
    }

    async fn handle_options(&self, tx: &mut Transaction) -> Result<()> {
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            if let Some(session) = self.inner.sessions.lock().unwrap().get_mut(&dialog_id) {
                session.last_activity = Instant::now();
            }
        }
        tx.reply(rsip::StatusCode::OK)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    async fn handle_ack(&self, tx: &mut Transaction) -> Result<()> {
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            let sessions = self.inner.sessions.lock().unwrap();
            if sessions.contains_key(&dialog_id) {
                info!("ACK received for dialog: {}", dialog_id);
            }
        }
        Ok(())
    }

    async fn handle_cancel(&self, tx: &mut Transaction) -> Result<()> {
        tx.reply(rsip::StatusCode::OK)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }
}

#[async_trait]
impl ProxyModule for CallModule {
    fn name(&self) -> &str {
        "call"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Bye,
            rsip::Method::Info,
            rsip::Method::Ack,
            rsip::Method::Cancel,
            rsip::Method::Options,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        info!("Call module started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        info!("Call module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(tx).await {
                    error!("Error handling INVITE: {}", e);
                    if tx.last_response.is_none() {
                        tx.reply_with(
                            rsip::StatusCode::ServerInternalError,
                            vec![rsip::Header::ErrorInfo(e.to_string().into())],
                            None,
                        )
                        .await
                        .map_err(|e| anyhow!(e))?;
                    }
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Bye => {
                if let Err(e) = self.handle_bye(tx).await {
                    error!("Error handling BYE: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Options => {
                if let Err(e) = self.handle_options(tx).await {
                    error!("Error handling OPTIONS: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Ack => {
                if let Err(e) = self.handle_ack(tx).await {
                    error!("Error handling ACK: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Cancel => {
                if let Err(e) = self.handle_cancel(tx).await {
                    error!("Error handling CANCEL: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }

    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}
