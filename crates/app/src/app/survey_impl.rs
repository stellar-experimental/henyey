//! Network survey: collecting and relaying topology survey data from peers.

use super::*;

/// Error returned when stopping a survey fails.
#[derive(Debug, thiserror::Error)]
pub enum SurveyStopError {
    #[error("no active survey to stop")]
    NoActiveSurvey,
}

/// Constructs and signs outgoing start/stop survey messages for a given nonce.
///
/// All methods are synchronous — no `.await` between ledger read and signing.
/// Each `build_*` reads `survey_local_ledger()` fresh (never cached), matching
/// stellar-core's per-emission ledger read (SurveyManager.cpp:293, 524).
///
/// Private to this module.
pub(super) struct SurveyMessageSigner<'a> {
    app: &'a App,
    nonce: u32,
}

impl<'a> SurveyMessageSigner<'a> {
    pub(super) fn new(app: &'a App, nonce: u32) -> Self {
        Self { app, nonce }
    }

    /// Build and sign a start-collecting message. Reads ledger fresh.
    /// Returns (signed, unsigned_inner) — caller needs inner for local state transition.
    pub(super) fn build_start(
        &self,
    ) -> anyhow::Result<(
        stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage,
        TimeSlicedSurveyStartCollectingMessage,
    )> {
        let ledger_num = self.app.survey_local_ledger();
        let start = TimeSlicedSurveyStartCollectingMessage {
            surveyor_id: self.app.local_node_id(),
            nonce: self.nonce,
            ledger_num,
        };
        let bytes = start
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map_err(|e| anyhow::anyhow!("Failed to encode survey start message: {e}"))?;
        let signature = self.app.sign_survey_message(&bytes);
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage {
            signature,
            start_collecting: start.clone(),
        };
        Ok((signed, start))
    }

    /// Build and sign a stop-collecting message. Reads ledger fresh.
    /// Returns (signed, unsigned_inner) — caller needs inner for local state transition.
    pub(super) fn build_stop(
        &self,
    ) -> anyhow::Result<(
        stellar_xdr::curr::SignedTimeSlicedSurveyStopCollectingMessage,
        TimeSlicedSurveyStopCollectingMessage,
    )> {
        let ledger_num = self.app.survey_local_ledger();
        let stop = TimeSlicedSurveyStopCollectingMessage {
            surveyor_id: self.app.local_node_id(),
            nonce: self.nonce,
            ledger_num,
        };
        let bytes = stop
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map_err(|e| anyhow::anyhow!("Failed to encode survey stop message: {e}"))?;
        let signature = self.app.sign_survey_message(&bytes);
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyStopCollectingMessage {
            signature,
            stop_collecting: stop.clone(),
        };
        Ok((signed, stop))
    }
}

/// Extends `SurveyMessageSigner` with request-building capability.
///
/// Constructed via async `new()` which resolves the encryption key upfront.
/// `build_request` is ONLY available on this type — compile-time safety prevents
/// calling it without first resolving the async prerequisite.
///
/// Private to this module.
pub(super) struct SurveyRequestSigner<'a> {
    base: SurveyMessageSigner<'a>,
    encryption_key: Curve25519Public,
}

impl<'a> SurveyRequestSigner<'a> {
    /// Resolve encryption key (async), then provide sync request building.
    pub(super) async fn new(app: &'a App, nonce: u32) -> Self {
        let secret = app.ensure_survey_secret(nonce).await;
        let public = CurvePublicKey::from(&secret);
        Self {
            base: SurveyMessageSigner::new(app, nonce),
            encryption_key: Curve25519Public {
                key: public.to_bytes(),
            },
        }
    }

    /// Build and sign a request message for a specific peer.
    ///
    /// **Reads `survey_local_ledger()` fresh on each call** — never caches.
    /// Matches stellar-core's per-request pattern (SurveyManager.cpp:217).
    pub(super) fn build_request(
        &self,
        peer_id: &henyey_overlay::PeerId,
        inbound_index: u32,
        outbound_index: u32,
    ) -> anyhow::Result<stellar_xdr::curr::SignedTimeSlicedSurveyRequestMessage> {
        let ledger_num = self.base.app.survey_local_ledger();
        let request = SurveyRequestMessage {
            surveyor_peer_id: self.base.app.local_node_id(),
            surveyed_peer_id: stellar_xdr::curr::NodeId(peer_id.0.clone()),
            ledger_num,
            encryption_key: self.encryption_key.clone(),
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };
        let message = TimeSlicedSurveyRequestMessage {
            request,
            nonce: self.base.nonce,
            inbound_peers_index: inbound_index,
            outbound_peers_index: outbound_index,
        };
        let bytes = message.to_xdr(stellar_xdr::curr::Limits::none())?;
        let signature = self.base.app.sign_survey_message(&bytes);
        Ok(stellar_xdr::curr::SignedTimeSlicedSurveyRequestMessage {
            request_signature: signature,
            request: message,
        })
    }
}

impl App {
    pub async fn survey_report(&self) -> SurveyReport {
        let (phase, nonce, local_node, inbound_peers, outbound_peers) = {
            let survey_state = self.survey_state.read().await;
            (
                survey_state.data().phase(),
                survey_state.data().nonce(),
                survey_state.data().final_node_data(),
                survey_state.data().final_inbound_peers().to_vec(),
                survey_state.data().final_outbound_peers().to_vec(),
            )
        };

        let (survey_in_progress, backlog, bad_response_nodes) = {
            let reporting = self.survey_reporting.read().await;
            let backlog = reporting
                .peers
                .iter()
                .map(|peer| peer.to_hex())
                .collect::<Vec<_>>();
            let bad = reporting
                .bad_response_nodes
                .iter()
                .map(|peer| peer.to_hex())
                .collect::<Vec<_>>();
            (reporting.running, backlog, bad)
        };
        let mut backlog = backlog;
        backlog.sort();
        let mut bad_response_nodes = bad_response_nodes;
        bad_response_nodes.sort();

        let peer_reports = {
            let results = self.survey_results.read().await;
            results
                .iter()
                .map(|(nonce, peers)| {
                    let mut reports = peers
                        .iter()
                        .map(|(peer_id, response)| SurveyPeerReport {
                            peer_id: peer_id.to_hex(),
                            response: response.clone(),
                        })
                        .collect::<Vec<_>>();
                    reports.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
                    (*nonce, reports)
                })
                .collect::<BTreeMap<_, _>>()
        };

        SurveyReport {
            phase,
            nonce,
            local_node,
            inbound_peers,
            outbound_peers,
            peer_reports,
            survey_in_progress,
            backlog,
            bad_response_nodes,
        }
    }

    pub async fn start_survey_collecting(&self, nonce: u32) -> anyhow::Result<()> {
        self.broadcast_survey_start(nonce).await
    }

    pub async fn stop_survey_collecting(&self) -> Result<(), SurveyStopError> {
        let nonce = { self.survey_state.read().await.data().nonce() };
        let Some(nonce) = nonce else {
            return Err(SurveyStopError::NoActiveSurvey);
        };
        self.broadcast_survey_stop(nonce).await;
        Ok(())
    }

    pub async fn stop_survey_reporting(&self) {
        {
            let mut reporting = self.survey_reporting.write().await;
            reporting.running = false;
        }

        if let Some(nonce) = self.survey_state.read().await.data().nonce() {
            self.survey_secrets.write().await.remove(&nonce);
        }
    }

    pub async fn survey_topology_timesliced(
        &self,
        peer_id: henyey_overlay::PeerId,
        inbound_index: u32,
        outbound_index: u32,
    ) -> bool {
        let start = self.start_survey_reporting().await;
        if start == SurveyReportingStart::NotReady {
            return false;
        }

        if let Some(nonce) = { self.survey_state.read().await.data().nonce() } {
            if let Some(peers) = self.survey_results.write().await.get_mut(&nonce) {
                peers.remove(&peer_id);
            }
        }

        let self_peer = henyey_overlay::PeerId::from_bytes(*self.keypair.public_key().as_bytes());
        let mut reporting = self.survey_reporting.write().await;
        if reporting.peers.contains(&peer_id) || peer_id == self_peer {
            return false;
        }
        reporting.bad_response_nodes.remove(&peer_id);
        reporting.peers.insert(peer_id.clone());
        reporting.queue.push_back(peer_id.clone());
        reporting
            .inbound_indices
            .insert(peer_id.clone(), inbound_index);
        reporting
            .outbound_indices
            .insert(peer_id.clone(), outbound_index);
        true
    }

    /// Test-only seam: seed the reporting backlog with a single peer and mark
    /// the reporting phase running, without driving the full collecting →
    /// reporting handshake (which requires an injected overlay and finalized
    /// node data). Used by the compat `/getsurveyresult` strkey-projection test
    /// (#3298) to get a deterministic non-empty `backlog`.
    #[cfg(test)]
    pub(crate) async fn seed_survey_reporting_backlog_for_test(
        &self,
        peer_id: henyey_overlay::PeerId,
    ) {
        let mut reporting = self.survey_reporting.write().await;
        reporting.running = true;
        reporting.peers.insert(peer_id.clone());
        reporting.queue.push_back(peer_id);
    }

    async fn start_survey_reporting(&self) -> SurveyReportingStart {
        let nonce = { self.survey_state.read().await.data().nonce() };
        let Some(nonce) = nonce else {
            return SurveyReportingStart::NotReady;
        };
        if self
            .survey_state
            .read()
            .await
            .data()
            .final_node_data()
            .is_none()
        {
            return SurveyReportingStart::NotReady;
        }

        let mut reporting = self.survey_reporting.write().await;
        if reporting.running {
            return SurveyReportingStart::AlreadyRunning;
        }
        reporting.running = true;
        reporting.peers.clear();
        reporting.queue.clear();
        reporting.inbound_indices.clear();
        reporting.outbound_indices.clear();
        reporting.bad_response_nodes.clear();
        reporting.next_topoff = self.clock.now();

        self.survey_results.write().await.clear();
        self.ensure_survey_secret(nonce).await;
        if let Some(response) = self.local_topology_response().await {
            let self_peer =
                henyey_overlay::PeerId::from_bytes(*self.keypair.public_key().as_bytes());
            self.survey_results
                .write()
                .await
                .entry(nonce)
                .or_insert_with(HashMap::new)
                .insert(self_peer, response);
        }
        SurveyReportingStart::Started
    }

    async fn local_topology_response(&self) -> Option<TopologyResponseBodyV2> {
        const MAX_PEERS: usize = 25;
        let survey_state = self.survey_state.read().await;
        let node_data = survey_state.data().final_node_data()?;
        let inbound_peers = survey_state
            .data()
            .final_inbound_peers()
            .iter()
            .take(MAX_PEERS)
            .cloned()
            .collect::<Vec<_>>();
        let outbound_peers = survey_state
            .data()
            .final_outbound_peers()
            .iter()
            .take(MAX_PEERS)
            .cloned()
            .collect::<Vec<_>>();
        Some(TopologyResponseBodyV2 {
            inbound_peers: TimeSlicedPeerDataList(inbound_peers.try_into().unwrap_or_default()),
            outbound_peers: TimeSlicedPeerDataList(outbound_peers.try_into().unwrap_or_default()),
            node_data,
        })
    }

    pub(super) async fn top_off_survey_requests(&self) {
        const MAX_REQUEST_LIMIT_PER_LEDGER: usize = 10;

        let (running, next_topoff) = {
            let reporting = self.survey_reporting.read().await;
            (reporting.running, reporting.next_topoff)
        };
        if !running {
            return;
        }
        if self.clock.now() < next_topoff {
            return;
        }

        let nonce = { self.survey_state.read().await.data().nonce() };
        let Some(nonce) = nonce else {
            self.stop_survey_reporting().await;
            return;
        };
        if !self
            .survey_state
            .read()
            .await
            .data()
            .nonce_is_reporting(nonce)
        {
            self.stop_survey_reporting().await;
            return;
        }

        let mut requests_sent = 0usize;
        let mut to_send = Vec::new();

        {
            let mut reporting = self.survey_reporting.write().await;
            while requests_sent < MAX_REQUEST_LIMIT_PER_LEDGER {
                let Some(peer_id) = reporting.queue.pop_front() else {
                    break;
                };
                if !reporting.peers.remove(&peer_id) {
                    continue;
                }
                let inbound_index = reporting.inbound_indices.remove(&peer_id).unwrap_or(0);
                let outbound_index = reporting.outbound_indices.remove(&peer_id).unwrap_or(0);
                to_send.push((peer_id, inbound_index, outbound_index));
                requests_sent += 1;
            }
            reporting.next_topoff = self.clock.now() + self.survey_throttle;
        }

        for (peer_id, inbound_index, outbound_index) in to_send {
            let ok = self
                .send_survey_request(peer_id.clone(), nonce, inbound_index, outbound_index)
                .await;
            if !ok {
                tracing::debug!(peer = %peer_id, "Survey request failed to send");
            }
        }
    }

    async fn send_survey_request(
        &self,
        peer_id: henyey_overlay::PeerId,
        nonce: u32,
        inbound_index: u32,
        outbound_index: u32,
    ) -> bool {
        let signer = SurveyRequestSigner::new(self, nonce).await;
        let signed = match signer.build_request(&peer_id, inbound_index, outbound_index) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to build survey request");
                return false;
            }
        };

        let local_node_id = self.local_node_id();
        let message_bytes = match signed.request.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(_) => return false,
        };

        let mut survey_state = self.survey_state.write().await;
        let ok = survey_state.add_and_validate_request(
            &signed.request.request,
            &local_node_id,
            nonce,
            || {
                // Self-originated survey: no relaying peer to drop (matches
                // stellar-core's `peer == nullptr` case).
                self.verify_survey_signature_or_drop(
                    &signed.request.request.surveyor_peer_id,
                    &message_bytes,
                    &signed.request_signature,
                    None,
                )
            },
        );
        drop(survey_state);
        if !ok {
            return false;
        }

        self.broadcast_survey_message(StellarMessage::TimeSlicedSurveyRequest(signed))
            .await
            .is_ok()
    }

    async fn broadcast_survey_start(&self, nonce: u32) -> anyhow::Result<()> {
        let signer = SurveyMessageSigner::new(self, nonce);
        let (signed, start) = signer.build_start()?;

        self.broadcast_survey_message(StellarMessage::TimeSlicedSurveyStartCollecting(signed))
            .await?;
        self.survey_results
            .write()
            .await
            .entry(nonce)
            .or_insert_with(HashMap::new);
        self.start_local_survey_collecting(&start).await;
        Ok(())
    }

    async fn broadcast_survey_stop(&self, nonce: u32) {
        let signer = SurveyMessageSigner::new(self, nonce);
        let (signed, stop) = match signer.build_stop() {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to build survey stop message");
                return;
            }
        };

        let _ = self
            .broadcast_survey_message(StellarMessage::TimeSlicedSurveyStopCollecting(signed))
            .await;
        self.stop_local_survey_collecting(&stop).await;
    }

    async fn broadcast_survey_message(&self, message: StellarMessage) -> anyhow::Result<()> {
        let overlay = self
            .overlay()
            .await
            .ok_or_else(|| anyhow::anyhow!("Overlay not available"))?;

        overlay
            .broadcast(message)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to broadcast survey message: {e}"))
    }

    async fn ensure_survey_secret(&self, nonce: u32) -> CurveSecretKey {
        if let Some(secret) = self.survey_secrets.read().await.get(&nonce).copied() {
            return CurveSecretKey::from(secret);
        }
        let secret = CurveSecretKey::random_from_rng(rand::rngs::OsRng);
        self.survey_secrets
            .write()
            .await
            .insert(nonce, secret.to_bytes());
        secret
    }

    pub(super) async fn handle_survey_start_collecting(
        &self,
        peer_id: &henyey_overlay::PeerId,
        signed: stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage,
    ) {
        let message = &signed.start_collecting;
        let message_bytes = match message.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encode survey start message");
                return;
            }
        };
        if !self.surveyor_permitted(&message.surveyor_id) {
            return;
        }
        let overlay = self.overlay().await;
        let is_valid = {
            let survey_state = self.survey_state.read().await;
            survey_state.validate_start_collecting(message, || {
                self.verify_survey_signature_or_drop(
                    &message.surveyor_id,
                    &message_bytes,
                    &signed.signature,
                    overlay.as_ref().map(|o| (o, peer_id)),
                )
            })
        };
        if !is_valid {
            tracing::debug!(peer = %peer_id, "Survey start rejected by limiter");
            return;
        }

        let (snapshots, added, dropped) = {
            let Some(overlay) = self.overlay().await else {
                return;
            };
            (
                overlay.peer_snapshots(),
                overlay.added_authenticated_peers(),
                overlay.dropped_authenticated_peers(),
            )
        };

        let (inbound, outbound) = Self::partition_peer_snapshots(snapshots);
        let lost_sync = self.lost_sync_count.load(Ordering::Relaxed);
        let state = self.state().await;
        let initially_out_of_sync = matches!(state, AppState::Initializing | AppState::CatchingUp);

        let node_stats = crate::survey::NodeStatsSnapshot {
            lost_sync_count: lost_sync,
            out_of_sync: initially_out_of_sync,
            added_peers: added,
            dropped_peers: dropped,
        };
        let mut survey_state = self.survey_state.write().await;
        if survey_state
            .data_mut()
            .start_collecting(message, &inbound, &outbound, node_stats)
        {
            tracing::debug!(peer = %peer_id, "Survey collection started");
        } else {
            tracing::debug!(peer = %peer_id, "Survey collection already active");
        }
    }

    pub(super) async fn handle_survey_stop_collecting(
        &self,
        peer_id: &henyey_overlay::PeerId,
        signed: stellar_xdr::curr::SignedTimeSlicedSurveyStopCollectingMessage,
    ) {
        let message = &signed.stop_collecting;
        let message_bytes = match message.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encode survey stop message");
                return;
            }
        };
        if !self.surveyor_permitted(&message.surveyor_id) {
            return;
        }
        let overlay = self.overlay().await;
        let is_valid = {
            let survey_state = self.survey_state.read().await;
            survey_state.validate_stop_collecting(message, || {
                self.verify_survey_signature_or_drop(
                    &message.surveyor_id,
                    &message_bytes,
                    &signed.signature,
                    overlay.as_ref().map(|o| (o, peer_id)),
                )
            })
        };
        if !is_valid {
            tracing::debug!(peer = %peer_id, "Survey stop rejected by limiter");
            return;
        }

        let (snapshots, added, dropped) = {
            let Some(overlay) = self.overlay().await else {
                return;
            };
            (
                overlay.peer_snapshots(),
                overlay.added_authenticated_peers(),
                overlay.dropped_authenticated_peers(),
            )
        };

        let (inbound, outbound) = Self::partition_peer_snapshots(snapshots);
        let lost_sync = self.lost_sync_count.load(Ordering::Relaxed);

        let mut survey_state = self.survey_state.write().await;
        if survey_state
            .data_mut()
            .stop_collecting(message, &inbound, &outbound, added, dropped, lost_sync)
        {
            tracing::debug!(peer = %peer_id, "Survey collection stopped");
        } else {
            tracing::debug!(peer = %peer_id, "Survey stop ignored (inactive or nonce mismatch)");
        }
    }

    pub(super) async fn handle_survey_request(
        &self,
        peer_id: &henyey_overlay::PeerId,
        signed: stellar_xdr::curr::SignedTimeSlicedSurveyRequestMessage,
    ) {
        let request = &signed.request;
        let request_bytes = match request.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encode survey request");
                return;
            }
        };

        if !self.surveyor_permitted(&request.request.surveyor_peer_id) {
            return;
        }

        let local_node_id = self.local_node_id();
        let overlay = self.overlay().await;
        let is_valid = {
            let mut survey_state = self.survey_state.write().await;
            survey_state.add_and_validate_request(
                &request.request,
                &local_node_id,
                request.nonce,
                || {
                    self.verify_survey_signature_or_drop(
                        &request.request.surveyor_peer_id,
                        &request_bytes,
                        &signed.request_signature,
                        overlay.as_ref().map(|o| (o, peer_id)),
                    )
                },
            )
        };
        if !is_valid {
            tracing::debug!(peer = %peer_id, "Survey request rejected by limiter");
            return;
        }

        if request.request.surveyed_peer_id != local_node_id {
            let _ = self
                .broadcast_survey_message(StellarMessage::TimeSlicedSurveyRequest(signed))
                .await;
            return;
        }
        let response_body = match request.request.command_type {
            stellar_xdr::curr::SurveyMessageCommandType::TimeSlicedSurveyTopology => {
                let survey_state = self.survey_state.read().await;
                match survey_state.data().fill_survey_data(request) {
                    Some(body) => body,
                    None => {
                        tracing::debug!(peer = %peer_id, "Survey request without reporting data");
                        return;
                    }
                }
            }
        };

        let response_body = SurveyResponseBody::SurveyTopologyResponseV2(response_body);
        let response_body_bytes = match response_body.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encode survey response body");
                return;
            }
        };
        let encrypted_body_bytes = match henyey_crypto::seal_to_curve25519_public_key(
            &request.request.encryption_key.key,
            &response_body_bytes,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encrypt survey response body");
                return;
            }
        };
        let encrypted_body = match encrypted_body_bytes.try_into() {
            Ok(body) => EncryptedBody(body),
            Err(_) => {
                tracing::debug!(peer = %peer_id, "Survey response body exceeded XDR limits");
                return;
            }
        };

        let response = SurveyResponseMessage {
            surveyor_peer_id: request.request.surveyor_peer_id.clone(),
            surveyed_peer_id: local_node_id,
            ledger_num: request.request.ledger_num,
            command_type: request.request.command_type,
            encrypted_body,
        };

        let response_message = TimeSlicedSurveyResponseMessage {
            response,
            nonce: request.nonce,
        };

        let response_bytes = match response_message.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encode survey response");
                return;
            }
        };

        let signature = self.sign_survey_message(&response_bytes);

        let signed_response = stellar_xdr::curr::SignedTimeSlicedSurveyResponseMessage {
            response_signature: signature,
            response: response_message,
        };

        if let Some(overlay) = self.overlay().await {
            if let Err(e) = overlay.try_send_to(
                peer_id,
                StellarMessage::TimeSlicedSurveyResponse(signed_response),
            ) {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to send survey response");
            }
        }
    }

    pub(super) async fn handle_survey_response(
        &self,
        peer_id: &henyey_overlay::PeerId,
        signed: SignedTimeSlicedSurveyResponseMessage,
    ) {
        let response_message = signed.response.clone();
        let response_bytes = match response_message.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encode survey response");
                return;
            }
        };

        let overlay = self.overlay().await;
        let is_valid = {
            let mut survey_state = self.survey_state.write().await;
            survey_state.record_and_validate_response(
                &response_message.response,
                response_message.nonce,
                || {
                    self.verify_survey_signature_or_drop(
                        &response_message.response.surveyed_peer_id,
                        &response_bytes,
                        &signed.response_signature,
                        overlay.as_ref().map(|o| (o, peer_id)),
                    )
                },
            )
        };
        if !is_valid {
            tracing::debug!(peer = %peer_id, "Survey response rejected by limiter");
            return;
        }

        if response_message.response.surveyor_peer_id != self.local_node_id() {
            let _ = self
                .broadcast_survey_message(StellarMessage::TimeSlicedSurveyResponse(signed))
                .await;
            return;
        }

        let secret = {
            self.survey_secrets
                .read()
                .await
                .get(&response_message.nonce)
                .copied()
        };

        let secret = match secret {
            Some(secret) => secret,
            None => {
                tracing::debug!(peer = %peer_id, "Survey response without matching secret");
                return;
            }
        };

        let decrypted = match henyey_crypto::open_from_curve25519_secret_key(
            &secret,
            response_message.response.encrypted_body.0.as_slice(),
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to decrypt survey response");
                let mut reporting = self.survey_reporting.write().await;
                reporting.bad_response_nodes.insert(peer_id.clone());
                return;
            }
        };

        let response_body = match SurveyResponseBody::from_xdr(
            decrypted.as_slice(),
            stellar_xdr::curr::Limits::none(),
        ) {
            Ok(body) => body,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to decode survey response body");
                let mut reporting = self.survey_reporting.write().await;
                reporting.bad_response_nodes.insert(peer_id.clone());
                return;
            }
        };

        let SurveyResponseBody::SurveyTopologyResponseV2(body) = response_body;
        let (inbound_len, outbound_len) = {
            let mut results = self.survey_results.write().await;
            let entry = results
                .entry(response_message.nonce)
                .or_insert_with(HashMap::new)
                .entry(peer_id.clone())
                .or_insert_with(|| body.clone());
            Self::merge_topology_response(entry, &body);
            (entry.inbound_peers.0.len(), entry.outbound_peers.0.len())
        };
        tracing::debug!(
            peer = %peer_id,
            inbound = body.inbound_peers.0.len(),
            outbound = body.outbound_peers.0.len(),
            "Decrypted survey response"
        );

        let needs_more_inbound = body.inbound_peers.0.len() == TIME_SLICED_PEERS_MAX;
        let needs_more_outbound = body.outbound_peers.0.len() == TIME_SLICED_PEERS_MAX;
        if (needs_more_inbound || needs_more_outbound) && self.survey_reporting.read().await.running
        {
            let next_inbound = inbound_len as u32;
            let next_outbound = outbound_len as u32;
            let _ = self
                .survey_topology_timesliced(peer_id.clone(), next_inbound, next_outbound)
                .await;
        }
    }

    fn local_node_id(&self) -> stellar_xdr::curr::NodeId {
        stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*self.keypair.public_key().as_bytes()),
        ))
    }

    pub(crate) fn survey_local_ledger(&self) -> u32 {
        let tracking = self.herder.tracking_consensus_ledger_index();
        if tracking.is_boot() {
            // Not yet tracking (boot/syncing state). Stellar-core asserts
            // non-boot in trackingConsensusLedgerIndex(); henyey gracefully
            // falls back to LCL. Outgoing survey entrypoints are gated by
            // app state, so this fallback primarily serves incoming message
            // validation during catchup.
            self.current_ledger_seq()
        } else {
            tracking.as_u32()
        }
    }

    fn partition_peer_snapshots(
        snapshots: Vec<PeerSnapshot>,
    ) -> (Vec<PeerSnapshot>, Vec<PeerSnapshot>) {
        let mut inbound = Vec::new();
        let mut outbound = Vec::new();

        for snapshot in snapshots {
            match snapshot.info.direction {
                henyey_overlay::ConnectionDirection::Inbound => inbound.push(snapshot),
                henyey_overlay::ConnectionDirection::Outbound => outbound.push(snapshot),
            }
        }

        (inbound, outbound)
    }

    fn select_survey_peers(
        snapshots: Vec<PeerSnapshot>,
        max_peers: usize,
    ) -> Vec<henyey_overlay::PeerId> {
        let (mut inbound, mut outbound) = Self::partition_peer_snapshots(snapshots);
        let mut sort_by_activity = |a: &PeerSnapshot, b: &PeerSnapshot| {
            b.stats
                .messages_received
                .cmp(&a.stats.messages_received)
                .then_with(|| b.info.connected_at.cmp(&a.info.connected_at))
                .then_with(|| a.info.peer_id.to_hex().cmp(&b.info.peer_id.to_hex()))
        };
        inbound.sort_by(&mut sort_by_activity);
        outbound.sort_by(&mut sort_by_activity);

        let mut selected = Vec::new();
        let mut inbound_idx = 0usize;
        let mut outbound_idx = 0usize;

        while selected.len() < max_peers
            && (inbound_idx < inbound.len() || outbound_idx < outbound.len())
        {
            if outbound_idx < outbound.len() {
                selected.push(outbound[outbound_idx].info.peer_id.clone());
                outbound_idx += 1;
                if selected.len() == max_peers {
                    break;
                }
            }
            if inbound_idx < inbound.len() {
                selected.push(inbound[inbound_idx].info.peer_id.clone());
                inbound_idx += 1;
            }
        }

        selected
    }

    fn sign_survey_message(&self, message: &[u8]) -> stellar_xdr::curr::Signature {
        let sig = self.keypair.sign(message);
        sig.into()
    }

    fn merge_topology_response(
        existing: &mut TopologyResponseBodyV2,
        incoming: &TopologyResponseBodyV2,
    ) {
        existing.node_data = incoming.node_data.clone();

        let mut inbound = existing.inbound_peers.0.iter().cloned().collect::<Vec<_>>();
        inbound.extend(incoming.inbound_peers.0.iter().cloned());
        existing.inbound_peers.0 = inbound.try_into().unwrap_or_default();

        let mut outbound = existing
            .outbound_peers
            .0
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        outbound.extend(incoming.outbound_peers.0.iter().cloned());
        existing.outbound_peers.0 = outbound.try_into().unwrap_or_default();
    }

    /// Verify a survey message's Ed25519 signature and, on failure, drop the
    /// relaying peer with `ERROR_MSG(ERR_MISC, "Survey has invalid signature")`.
    ///
    /// Matches stellar-core `SurveyManager::dropPeerIfSigInvalid`
    /// (SurveyManager.cpp:807-819): the drop is performed *only* on a genuine
    /// signature-verification failure, and only when a relaying peer is present.
    /// Self-originated surveys (`send_survey_request`) pass `None` — matching
    /// stellar-core's `peer == nullptr` case, which does not drop.
    ///
    /// This helper is invoked from inside the limiter's success-validation
    /// closure, which the limiter only calls after its non-signature gates
    /// (ledger range, duplicate, max-request) and the nonce check pass. So a
    /// `false` return here is unambiguously a signature failure — it can never
    /// be confused with a rate-limit/nonce/duplicate rejection, and the drop
    /// therefore never over-penalizes a peer relaying a validly-signed-but-
    /// rejected message.
    fn verify_survey_signature_or_drop(
        &self,
        node_id: &stellar_xdr::curr::NodeId,
        message: &[u8],
        signature: &stellar_xdr::curr::Signature,
        relaying_peer: Option<(&Arc<OverlayManager>, &henyey_overlay::PeerId)>,
    ) -> bool {
        if self.verify_survey_signature(node_id, message, signature) {
            return true;
        }
        if let Some((overlay, peer_id)) = relaying_peer {
            overlay.send_error_and_drop(
                peer_id,
                stellar_xdr::curr::ErrorCode::Misc,
                "Survey has invalid signature",
            );
        }
        false
    }

    fn verify_survey_signature(
        &self,
        node_id: &stellar_xdr::curr::NodeId,
        message: &[u8],
        signature: &stellar_xdr::curr::Signature,
    ) -> bool {
        let key_bytes = match Self::node_id_bytes(node_id) {
            Some(bytes) => bytes,
            None => return false,
        };
        let sig = match henyey_crypto::Signature::try_from(signature) {
            Ok(sig) => sig,
            Err(_) => return false,
        };
        henyey_crypto::verify_from_raw_key(&key_bytes, message, &sig).is_ok()
    }

    fn node_id_bytes(node_id: &stellar_xdr::curr::NodeId) -> Option<[u8; 32]> {
        match &node_id.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => Some(key.0),
        }
    }

    fn surveyor_permitted(&self, surveyor_id: &stellar_xdr::curr::NodeId) -> bool {
        let allowed_keys = &self.config.overlay.surveyor_keys;
        if allowed_keys.is_empty() {
            let quorum_nodes = self.herder.local_quorum_nodes();
            if quorum_nodes.is_empty() {
                return false;
            }
            return quorum_nodes.contains(surveyor_id);
        }

        let Some(bytes) = Self::node_id_bytes(surveyor_id) else {
            return false;
        };

        allowed_keys.iter().any(|key| {
            henyey_crypto::PublicKey::from_strkey(key)
                .map(|pk| pk.as_bytes() == &bytes)
                .unwrap_or(false)
        })
    }

    pub(super) async fn advance_survey_scheduler(&self) {
        const SURVEY_INTERVAL: Duration = Duration::from_secs(60);
        const SURVEY_COLLECT_DELAY: Duration = Duration::from_secs(5);
        const SURVEY_RESPONSE_WAIT: Duration = Duration::from_secs(5);
        const SURVEY_MAX_PEERS: usize = 4;

        let now = self.clock.now();

        // Phase 1: Snapshot scheduler state under a short lock.
        let action = {
            let scheduler = self.survey_scheduler.lock().await;
            if now < scheduler.next_action {
                SchedulerAction::NotDue
            } else {
                match scheduler.phase {
                    SurveySchedulerPhase::Idle => SchedulerAction::Idle {
                        last_started: scheduler.last_started,
                    },
                    SurveySchedulerPhase::StartSent => SchedulerAction::StartSent {
                        peers: scheduler.peers.clone(),
                        nonce: scheduler.nonce,
                    },
                    SurveySchedulerPhase::RequestSent => SchedulerAction::RequestSent {
                        peers: scheduler.peers.clone(),
                        nonce: scheduler.nonce,
                    },
                }
            }
        }; // lock dropped

        // Phase 2: Perform async work without any lock held.
        match action {
            SchedulerAction::NotDue => {}

            SchedulerAction::Idle { last_started } => {
                if self.survey_state.read().await.data().survey_is_active()
                    || self.survey_reporting.read().await.running
                {
                    let mut scheduler = self.survey_scheduler.lock().await;
                    debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::Idle);
                    scheduler.next_action = now + SURVEY_INTERVAL;
                    return;
                }
                let state = *self.state.read().await;
                if !matches!(state, AppState::Synced | AppState::Validating) {
                    let mut scheduler = self.survey_scheduler.lock().await;
                    debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::Idle);
                    scheduler.next_action = now + SURVEY_INTERVAL;
                    return;
                }
                if let Some(last) = last_started {
                    if now.duration_since(last) < self.survey_throttle {
                        let mut scheduler = self.survey_scheduler.lock().await;
                        debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::Idle);
                        scheduler.next_action = last + self.survey_throttle;
                        return;
                    }
                }

                let peers = {
                    let Some(overlay) = self.overlay().await else {
                        let mut scheduler = self.survey_scheduler.lock().await;
                        debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::Idle);
                        scheduler.next_action = now + SURVEY_INTERVAL;
                        return;
                    };
                    Self::select_survey_peers(overlay.peer_snapshots(), SURVEY_MAX_PEERS)
                };

                if peers.is_empty() {
                    let mut scheduler = self.survey_scheduler.lock().await;
                    debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::Idle);
                    scheduler.next_action = now + SURVEY_INTERVAL;
                    return;
                }

                let nonce = {
                    let mut nonce_guard = self.survey_nonce.write().await;
                    let current = *nonce_guard;
                    *nonce_guard = nonce_guard.wrapping_add(1);
                    current
                };

                if !self.send_survey_start(&peers, nonce).await {
                    let mut scheduler = self.survey_scheduler.lock().await;
                    debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::Idle);
                    scheduler.next_action = now + SURVEY_INTERVAL;
                    return;
                }

                // Phase 3: Write back state under short lock.
                let mut scheduler = self.survey_scheduler.lock().await;
                debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::Idle);
                scheduler.phase = SurveySchedulerPhase::StartSent;
                scheduler.peers = peers;
                scheduler.nonce = nonce;
                scheduler.next_action = now + SURVEY_COLLECT_DELAY;
                scheduler.last_started = Some(now);
            }

            SchedulerAction::StartSent { peers, nonce } => {
                if !self.send_survey_requests(&peers, nonce).await {
                    // Clean up: remove secrets AND stop local survey collection +
                    // clear results to prevent survey_is_active() from wedging
                    // future Idle ticks.
                    self.survey_secrets.write().await.remove(&nonce);
                    self.survey_results.write().await.remove(&nonce);
                    self.stop_local_survey_collecting_by_nonce(nonce).await;

                    let mut scheduler = self.survey_scheduler.lock().await;
                    debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::StartSent);
                    scheduler.phase = SurveySchedulerPhase::Idle;
                    scheduler.next_action = now + SURVEY_INTERVAL;
                    return;
                }

                let mut scheduler = self.survey_scheduler.lock().await;
                debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::StartSent);
                scheduler.phase = SurveySchedulerPhase::RequestSent;
                scheduler.next_action = now + SURVEY_RESPONSE_WAIT;
            }

            SchedulerAction::RequestSent { peers, nonce } => {
                self.send_survey_stop(&peers, nonce).await;
                for peer_id in &peers {
                    let _ = self.survey_topology_timesliced(peer_id.clone(), 0, 0).await;
                }

                let mut scheduler = self.survey_scheduler.lock().await;
                debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::RequestSent);
                scheduler.phase = SurveySchedulerPhase::Idle;
                scheduler.peers.clear();
                scheduler.nonce = 0;
                scheduler.next_action = now + SURVEY_INTERVAL;
            }
        }
    }

    pub(super) async fn update_survey_phase(&self) {
        let (snapshots, added, dropped) = {
            let Some(overlay) = self.overlay().await else {
                return;
            };
            (
                overlay.peer_snapshots(),
                overlay.added_authenticated_peers(),
                overlay.dropped_authenticated_peers(),
            )
        };

        let (inbound, outbound) = Self::partition_peer_snapshots(snapshots);
        let lost_sync = self.lost_sync_count.load(Ordering::Relaxed);

        let mut survey_state = self.survey_state.write().await;
        let last_closed = self.current_ledger_seq();
        survey_state.update_phase_and_clear(
            &inbound,
            &outbound,
            added,
            dropped,
            lost_sync,
            last_closed,
        );
    }

    async fn send_survey_start(&self, peers: &[henyey_overlay::PeerId], nonce: u32) -> bool {
        let signer = SurveyMessageSigner::new(self, nonce);
        let (signed, start) = match signer.build_start() {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to build survey start message");
                return false;
            }
        };

        let sent = self
            .send_survey_message(
                peers,
                StellarMessage::TimeSlicedSurveyStartCollecting(signed),
            )
            .await;
        if sent {
            self.survey_results
                .write()
                .await
                .entry(nonce)
                .or_insert_with(HashMap::new);
            self.start_local_survey_collecting(&start).await;
        }
        sent
    }

    async fn send_survey_requests(&self, peers: &[henyey_overlay::PeerId], nonce: u32) -> bool {
        let signer = SurveyRequestSigner::new(self, nonce).await;

        let mut ok = true;
        for peer in peers {
            let signed = match signer.build_request(peer, 0, 0) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %peer, error = %e, "Failed to build survey request");
                    ok = false;
                    continue;
                }
            };

            if !self
                .send_survey_message(
                    std::slice::from_ref(peer),
                    StellarMessage::TimeSlicedSurveyRequest(signed),
                )
                .await
            {
                ok = false;
            }
        }
        ok
    }

    async fn send_survey_stop(&self, peers: &[henyey_overlay::PeerId], nonce: u32) {
        let signer = SurveyMessageSigner::new(self, nonce);
        let (signed, stop) = match signer.build_stop() {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to build survey stop message");
                return;
            }
        };

        let _ = self
            .send_survey_message(
                peers,
                StellarMessage::TimeSlicedSurveyStopCollecting(signed),
            )
            .await;
        self.stop_local_survey_collecting(&stop).await;
    }

    async fn send_survey_message(
        &self,
        peers: &[henyey_overlay::PeerId],
        message: StellarMessage,
    ) -> bool {
        let Some(overlay) = self.overlay().await else {
            return false;
        };

        let mut ok = true;
        for peer in peers {
            if let Err(e) = overlay.try_send_to(peer, message.clone()) {
                tracing::debug!(peer = %peer, error = %e, "Failed to send survey message");
                ok = false;
            }
        }
        ok
    }

    async fn start_local_survey_collecting(
        &self,
        message: &TimeSlicedSurveyStartCollectingMessage,
    ) {
        let (snapshots, added, dropped) = {
            let Some(overlay) = self.overlay().await else {
                return;
            };
            (
                overlay.peer_snapshots(),
                overlay.added_authenticated_peers(),
                overlay.dropped_authenticated_peers(),
            )
        };

        let (inbound, outbound) = Self::partition_peer_snapshots(snapshots);
        let lost_sync = self.lost_sync_count.load(Ordering::Relaxed);
        let state = self.state().await;
        let initially_out_of_sync = matches!(state, AppState::Initializing | AppState::CatchingUp);

        let node_stats = crate::survey::NodeStatsSnapshot {
            lost_sync_count: lost_sync,
            out_of_sync: initially_out_of_sync,
            added_peers: added,
            dropped_peers: dropped,
        };
        let mut survey_state = self.survey_state.write().await;
        let _ = survey_state
            .data_mut()
            .start_collecting(message, &inbound, &outbound, node_stats);
    }

    async fn stop_local_survey_collecting(&self, message: &TimeSlicedSurveyStopCollectingMessage) {
        let (snapshots, added, dropped) = {
            let Some(overlay) = self.overlay().await else {
                return;
            };
            (
                overlay.peer_snapshots(),
                overlay.added_authenticated_peers(),
                overlay.dropped_authenticated_peers(),
            )
        };

        let (inbound, outbound) = Self::partition_peer_snapshots(snapshots);
        let lost_sync = self.lost_sync_count.load(Ordering::Relaxed);

        let mut survey_state = self.survey_state.write().await;
        let _ = survey_state
            .data_mut()
            .stop_collecting(message, &inbound, &outbound, added, dropped, lost_sync);
    }

    /// Stop local survey collecting by nonce, without requiring a network
    /// message struct. Used by the scheduler error path where constructing a
    /// full wire message is unnecessary. If overlay is unavailable, still
    /// transitions SurveyDataManager phase with empty peer data to prevent
    /// wedging.
    async fn stop_local_survey_collecting_by_nonce(&self, nonce: u32) {
        let surveyor_id = self.local_node_id();

        let (inbound, outbound, added, dropped, lost_sync) =
            if let Some(overlay) = self.overlay().await {
                let snapshots = overlay.peer_snapshots();
                let added = overlay.added_authenticated_peers();
                let dropped = overlay.dropped_authenticated_peers();
                let (inbound, outbound) = Self::partition_peer_snapshots(snapshots);
                let lost_sync = self.lost_sync_count.load(Ordering::Relaxed);
                (inbound, outbound, added, dropped, lost_sync)
            } else {
                // No overlay — use empty peer data to still transition phase.
                (vec![], vec![], 0, 0, 0)
            };

        let mut survey_state = self.survey_state.write().await;
        let _ = survey_state.data_mut().stop_collecting_by_identity(
            nonce,
            &surveyor_id,
            &inbound,
            &outbound,
            added,
            dropped,
            lost_sync,
        );
    }
}

#[cfg(test)]
mod survey_invalid_signature_tests {
    use super::*;

    /// Build an App whose configured surveyor key is the public key of
    /// `surveyor_secret`, with an injected overlay holding one test peer.
    /// Returns (app, relaying_peer_id, TestPeerReceiver).
    async fn app_with_permitted_surveyor(
        surveyor_secret: &henyey_crypto::SecretKey,
    ) -> (
        App,
        henyey_overlay::PeerId,
        henyey_overlay::TestPeerReceiver,
    ) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        // Permit the surveyor by its public strkey so `surveyor_permitted`
        // returns true and the limiter reaches the signature closure.
        config.overlay.surveyor_keys = vec![surveyor_secret.public_key().to_strkey()];
        let app = App::new(config).await.unwrap();
        // Keep the tempdir alive for the App's lifetime.
        std::mem::forget(dir);

        let overlay = OverlayManager::new(
            OverlayManagerConfig::default(),
            LocalNode::new_testnet(henyey_crypto::SecretKey::generate()),
        )
        .unwrap();
        let peer_id = PeerId::from_bytes([0x11; 32]);
        let receiver = overlay.inject_test_peer(peer_id.clone(), 16);
        *app.overlay.write().await = Some(Arc::new(overlay));

        (app, peer_id, receiver)
    }

    fn surveyor_node_id(secret: &henyey_crypto::SecretKey) -> stellar_xdr::curr::NodeId {
        stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*secret.public_key().as_bytes()),
        ))
    }

    /// Build a start-collecting message for the given surveyor at the app's
    /// current survey ledger, signed by `signing_secret` (which may differ
    /// from the surveyor to forge an invalid signature).
    fn build_signed_start(
        app: &App,
        surveyor_secret: &henyey_crypto::SecretKey,
        signing_secret: &henyey_crypto::SecretKey,
    ) -> stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage {
        let start = stellar_xdr::curr::TimeSlicedSurveyStartCollectingMessage {
            surveyor_id: surveyor_node_id(surveyor_secret),
            nonce: 1,
            ledger_num: app.survey_local_ledger(),
        };
        let bytes = start.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
        let signature: stellar_xdr::curr::Signature = signing_secret.sign(&bytes).into();
        stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage {
            signature,
            start_collecting: start,
        }
    }

    /// Regression for #3071: an authenticated peer relaying a survey message
    /// with an invalid signature must be dropped with
    /// `ERROR_MSG(Misc, "Survey has invalid signature")`. On main the handler
    /// only logs and returns — the peer stays connected and the channel is
    /// empty.
    #[tokio::test]
    async fn test_survey_invalid_signature_drops_peer() {
        let surveyor = henyey_crypto::SecretKey::generate();
        let forger = henyey_crypto::SecretKey::generate();
        let (app, peer_id, mut receiver) = app_with_permitted_surveyor(&surveyor).await;

        // Signed by a DIFFERENT key than the surveyor → invalid signature.
        let signed = build_signed_start(&app, &surveyor, &forger);
        app.handle_survey_start_collecting(&peer_id, signed).await;

        match receiver.try_recv() {
            Some(stellar_xdr::curr::StellarMessage::ErrorMsg(err)) => {
                assert_eq!(err.code, stellar_xdr::curr::ErrorCode::Misc);
                assert_eq!(err.msg.to_string(), "Survey has invalid signature");
            }
            other => {
                panic!("expected ErrorMsg(Misc, \"Survey has invalid signature\"), got {other:?}")
            }
        }
    }

    /// Guard against over-drop: a correctly-signed survey message must NOT
    /// drop the relaying peer.
    #[tokio::test]
    async fn test_survey_valid_signature_does_not_drop_peer() {
        let surveyor = henyey_crypto::SecretKey::generate();
        let (app, peer_id, mut receiver) = app_with_permitted_surveyor(&surveyor).await;

        // Signed by the surveyor itself → valid signature.
        let signed = build_signed_start(&app, &surveyor, &surveyor);
        app.handle_survey_start_collecting(&peer_id, signed).await;

        assert!(
            receiver.try_recv().is_none(),
            "valid-signature survey must not send an ERROR_MSG or drop the peer"
        );
    }

    /// Over-drop guard: a message rejected by the limiter for a non-signature
    /// reason (here, an out-of-range ledger number) must NOT drop the peer,
    /// even though the closure never runs and the message is dropped silently.
    #[tokio::test]
    async fn test_survey_limiter_rejection_does_not_drop_peer() {
        let surveyor = henyey_crypto::SecretKey::generate();
        let (app, peer_id, mut receiver) = app_with_permitted_surveyor(&surveyor).await;

        // Build a VALIDLY-signed message but with a wildly out-of-range ledger
        // so the limiter's ledger gate rejects it before the signature closure.
        let start = stellar_xdr::curr::TimeSlicedSurveyStartCollectingMessage {
            surveyor_id: surveyor_node_id(&surveyor),
            nonce: 1,
            ledger_num: 1_000_000,
        };
        let bytes = start.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
        let signature: stellar_xdr::curr::Signature = surveyor.sign(&bytes).into();
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage {
            signature,
            start_collecting: start,
        };
        app.handle_survey_start_collecting(&peer_id, signed).await;

        assert!(
            receiver.try_recv().is_none(),
            "limiter (non-signature) rejection must not drop the peer"
        );
    }

    /// `verify_survey_signature_or_drop` with `None` relaying peer (self-survey)
    /// must not panic and must not attempt a drop on invalid signature.
    #[tokio::test]
    async fn test_verify_survey_signature_or_drop_none_peer_no_drop() {
        let surveyor = henyey_crypto::SecretKey::generate();
        let forger = henyey_crypto::SecretKey::generate();
        let (app, _peer_id, _receiver) = app_with_permitted_surveyor(&surveyor).await;

        let message = b"some survey bytes";
        let signature: stellar_xdr::curr::Signature = forger.sign(message).into();
        // Invalid signature (wrong key), but None peer → returns false, no drop.
        let result = app.verify_survey_signature_or_drop(
            &surveyor_node_id(&surveyor),
            message,
            &signature,
            None,
        );
        assert!(!result, "forged signature must verify as invalid");
    }
}
