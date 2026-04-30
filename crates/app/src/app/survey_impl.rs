//! Network survey: collecting and relaying topology survey data from peers.

use super::*;

/// Error returned when stopping a survey fails.
#[derive(Debug, thiserror::Error)]
pub enum SurveyStopError {
    #[error("no active survey to stop")]
    NoActiveSurvey,
}

impl App {
    pub async fn survey_report(&self) -> SurveyReport {
        let (phase, nonce, local_node, inbound_peers, outbound_peers) = {
            let survey_data = self.survey_data.read().await;
            (
                survey_data.phase(),
                survey_data.nonce(),
                survey_data.final_node_data(),
                survey_data.final_inbound_peers().to_vec(),
                survey_data.final_outbound_peers().to_vec(),
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
        let ledger_num = self.survey_local_ledger().await;
        self.broadcast_survey_start(nonce, ledger_num).await
    }

    pub async fn stop_survey_collecting(&self) -> Result<(), SurveyStopError> {
        let ledger_num = self.survey_local_ledger().await;
        let nonce = { self.survey_data.read().await.nonce() };
        let Some(nonce) = nonce else {
            return Err(SurveyStopError::NoActiveSurvey);
        };
        self.broadcast_survey_stop(nonce, ledger_num).await;
        Ok(())
    }

    pub async fn stop_survey_reporting(&self) {
        {
            let mut reporting = self.survey_reporting.write().await;
            reporting.running = false;
        }

        if let Some(nonce) = self.survey_data.read().await.nonce() {
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

        if let Some(nonce) = { self.survey_data.read().await.nonce() } {
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

    async fn start_survey_reporting(&self) -> SurveyReportingStart {
        let nonce = { self.survey_data.read().await.nonce() };
        let Some(nonce) = nonce else {
            return SurveyReportingStart::NotReady;
        };
        if self.survey_data.read().await.final_node_data().is_none() {
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
        let survey_data = self.survey_data.read().await;
        let node_data = survey_data.final_node_data()?;
        let inbound_peers = survey_data
            .final_inbound_peers()
            .iter()
            .take(MAX_PEERS)
            .cloned()
            .collect::<Vec<_>>();
        let outbound_peers = survey_data
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

        let nonce = { self.survey_data.read().await.nonce() };
        let Some(nonce) = nonce else {
            self.stop_survey_reporting().await;
            return;
        };
        if !self.survey_data.read().await.nonce_is_reporting(nonce) {
            self.stop_survey_reporting().await;
            return;
        }

        let ledger_num = self.survey_local_ledger().await;
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
                .send_survey_request(
                    peer_id.clone(),
                    nonce,
                    ledger_num,
                    inbound_index,
                    outbound_index,
                )
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
        ledger_num: u32,
        inbound_index: u32,
        outbound_index: u32,
    ) -> bool {
        let local_node_id = self.local_node_id();
        let secret = self.ensure_survey_secret(nonce).await;
        let public = CurvePublicKey::from(&secret);
        let encryption_key = Curve25519Public {
            key: public.to_bytes(),
        };

        let request = SurveyRequestMessage {
            surveyor_peer_id: local_node_id.clone(),
            surveyed_peer_id: stellar_xdr::curr::NodeId(peer_id.0.clone()),
            ledger_num,
            encryption_key,
            command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
        };

        let message = TimeSlicedSurveyRequestMessage {
            request,
            nonce,
            inbound_peers_index: inbound_index,
            outbound_peers_index: outbound_index,
        };

        let message_bytes = match message.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(peer = %peer_id, error = %e, "Failed to encode survey request");
                return false;
            }
        };

        let signature = self.sign_survey_message(&message_bytes);
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyRequestMessage {
            request_signature: signature,
            request: message,
        };

        let local_ledger = self.survey_local_ledger().await;
        let mut limiter = self.survey_limiter.write().await;
        let ok = limiter.add_and_validate_request(
            &signed.request.request,
            local_ledger,
            &local_node_id,
            || {
                self.verify_survey_signature(
                    &signed.request.request.surveyor_peer_id,
                    &message_bytes,
                    &signed.request_signature,
                )
            },
        );
        if !ok {
            return false;
        }

        self.broadcast_survey_message(StellarMessage::TimeSlicedSurveyRequest(signed))
            .await
            .is_ok()
    }

    async fn broadcast_survey_start(&self, nonce: u32, ledger_num: u32) -> anyhow::Result<()> {
        let start = TimeSlicedSurveyStartCollectingMessage {
            surveyor_id: self.local_node_id(),
            nonce,
            ledger_num,
        };
        let start_bytes = start
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map_err(|e| anyhow::anyhow!("Failed to encode survey start message: {e}"))?;
        let signature = self.sign_survey_message(&start_bytes);
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage {
            signature,
            start_collecting: start.clone(),
        };

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

    async fn broadcast_survey_stop(&self, nonce: u32, ledger_num: u32) {
        let stop = TimeSlicedSurveyStopCollectingMessage {
            surveyor_id: self.local_node_id(),
            nonce,
            ledger_num,
        };

        let stop_bytes = match stop.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to encode survey stop message");
                return;
            }
        };

        let signature = self.sign_survey_message(&stop_bytes);
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyStopCollectingMessage {
            signature,
            stop_collecting: stop.clone(),
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
        let local_ledger = self.survey_local_ledger().await;
        let survey_active = { self.survey_data.read().await.survey_is_active() };
        let limiter = self.survey_limiter.read().await;
        let is_valid =
            limiter.validate_start_collecting(message, local_ledger, survey_active, || {
                self.verify_survey_signature(
                    &message.surveyor_id,
                    &message_bytes,
                    &signed.signature,
                )
            });
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
        let mut survey_data = self.survey_data.write().await;
        if survey_data.start_collecting(message, &inbound, &outbound, node_stats) {
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
        let local_ledger = self.survey_local_ledger().await;
        let limiter = self.survey_limiter.read().await;
        let is_valid = limiter.validate_stop_collecting(message, local_ledger, || {
            self.verify_survey_signature(&message.surveyor_id, &message_bytes, &signed.signature)
        });
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

        let mut survey_data = self.survey_data.write().await;
        if survey_data.stop_collecting(message, &inbound, &outbound, added, dropped, lost_sync) {
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
        let local_ledger = self.survey_local_ledger().await;
        let nonce_is_reporting = self
            .survey_data
            .read()
            .await
            .nonce_is_reporting(request.nonce);
        let mut limiter = self.survey_limiter.write().await;
        let is_valid = limiter.add_and_validate_request(
            &request.request,
            local_ledger,
            &local_node_id,
            || {
                nonce_is_reporting
                    && self.verify_survey_signature(
                        &request.request.surveyor_peer_id,
                        &request_bytes,
                        &signed.request_signature,
                    )
            },
        );
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
                let survey_data = self.survey_data.read().await;
                match survey_data.fill_survey_data(request) {
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

        let local_ledger = self.survey_local_ledger().await;
        let nonce_is_reporting = self
            .survey_data
            .read()
            .await
            .nonce_is_reporting(response_message.nonce);
        let mut limiter = self.survey_limiter.write().await;
        let is_valid =
            limiter.record_and_validate_response(&response_message.response, local_ledger, || {
                nonce_is_reporting
                    && self.verify_survey_signature(
                        &response_message.response.surveyed_peer_id,
                        &response_bytes,
                        &signed.response_signature,
                    )
            });
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

    async fn survey_local_ledger(&self) -> u32 {
        let tracking = self.herder.tracking_slot() as u32;
        if tracking == 0 {
            self.current_ledger_seq()
        } else {
            tracking
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
                        ledger_num: scheduler.ledger_num,
                    },
                    SurveySchedulerPhase::RequestSent => SchedulerAction::RequestSent {
                        peers: scheduler.peers.clone(),
                        nonce: scheduler.nonce,
                        ledger_num: scheduler.ledger_num,
                    },
                }
            }
        }; // lock dropped

        // Phase 2: Perform async work without any lock held.
        match action {
            SchedulerAction::NotDue => {}

            SchedulerAction::Idle { last_started } => {
                if self.survey_data.read().await.survey_is_active()
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

                let ledger_num = self.current_ledger_seq();
                let nonce = {
                    let mut nonce_guard = self.survey_nonce.write().await;
                    let current = *nonce_guard;
                    *nonce_guard = nonce_guard.wrapping_add(1);
                    current
                };

                if !self.send_survey_start(&peers, nonce, ledger_num).await {
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
                scheduler.ledger_num = ledger_num;
                scheduler.next_action = now + SURVEY_COLLECT_DELAY;
                scheduler.last_started = Some(now);
            }

            SchedulerAction::StartSent {
                peers,
                nonce,
                ledger_num,
            } => {
                if !self.send_survey_requests(&peers, nonce, ledger_num).await {
                    // Clean up: remove secrets AND stop local survey collection +
                    // clear results to prevent survey_is_active() from wedging
                    // future Idle ticks.
                    self.survey_secrets.write().await.remove(&nonce);
                    self.survey_results.write().await.remove(&nonce);
                    let stop = TimeSlicedSurveyStopCollectingMessage {
                        surveyor_id: self.local_node_id(),
                        nonce,
                        ledger_num,
                    };
                    self.stop_local_survey_collecting(&stop).await;

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

            SchedulerAction::RequestSent {
                peers,
                nonce,
                ledger_num,
            } => {
                self.send_survey_stop(&peers, nonce, ledger_num).await;
                for peer_id in &peers {
                    let _ = self.survey_topology_timesliced(peer_id.clone(), 0, 0).await;
                }

                let mut scheduler = self.survey_scheduler.lock().await;
                debug_assert_eq!(scheduler.phase, SurveySchedulerPhase::RequestSent);
                scheduler.phase = SurveySchedulerPhase::Idle;
                scheduler.peers.clear();
                scheduler.nonce = 0;
                scheduler.ledger_num = 0;
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

        let mut survey_data = self.survey_data.write().await;
        survey_data.update_phase(&inbound, &outbound, added, dropped, lost_sync);

        let last_closed = self.current_ledger_seq();
        let mut limiter = self.survey_limiter.write().await;
        limiter.clear_old_ledgers(last_closed);
    }

    async fn send_survey_start(
        &self,
        peers: &[henyey_overlay::PeerId],
        nonce: u32,
        ledger_num: u32,
    ) -> bool {
        let start = TimeSlicedSurveyStartCollectingMessage {
            surveyor_id: self.local_node_id(),
            nonce,
            ledger_num,
        };

        let start_bytes = match start.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to encode survey start message");
                return false;
            }
        };

        let signature = self.sign_survey_message(&start_bytes);
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyStartCollectingMessage {
            signature,
            start_collecting: start.clone(),
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

    async fn send_survey_requests(
        &self,
        peers: &[henyey_overlay::PeerId],
        nonce: u32,
        ledger_num: u32,
    ) -> bool {
        let local_node_id = self.local_node_id();
        let secret = self.ensure_survey_secret(nonce).await;
        let public = CurvePublicKey::from(&secret);
        let encryption_key = Curve25519Public {
            key: public.to_bytes(),
        };

        let mut ok = true;
        for peer in peers {
            let request = SurveyRequestMessage {
                surveyor_peer_id: local_node_id.clone(),
                surveyed_peer_id: stellar_xdr::curr::NodeId(peer.0.clone()),
                ledger_num,
                encryption_key: encryption_key.clone(),
                command_type: SurveyMessageCommandType::TimeSlicedSurveyTopology,
            };

            let message = TimeSlicedSurveyRequestMessage {
                request,
                nonce,
                inbound_peers_index: 0,
                outbound_peers_index: 0,
            };

            let message_bytes = match message.to_xdr(stellar_xdr::curr::Limits::none()) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::debug!(peer = %peer, error = %e, "Failed to encode survey request");
                    ok = false;
                    continue;
                }
            };

            let signature = self.sign_survey_message(&message_bytes);
            let signed = stellar_xdr::curr::SignedTimeSlicedSurveyRequestMessage {
                request_signature: signature,
                request: message,
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

    async fn send_survey_stop(
        &self,
        peers: &[henyey_overlay::PeerId],
        nonce: u32,
        ledger_num: u32,
    ) {
        let stop = TimeSlicedSurveyStopCollectingMessage {
            surveyor_id: self.local_node_id(),
            nonce,
            ledger_num,
        };

        let stop_bytes = match stop.to_xdr(stellar_xdr::curr::Limits::none()) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to encode survey stop message");
                return;
            }
        };

        let signature = self.sign_survey_message(&stop_bytes);
        let signed = stellar_xdr::curr::SignedTimeSlicedSurveyStopCollectingMessage {
            signature,
            stop_collecting: stop.clone(),
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
        let mut survey_data = self.survey_data.write().await;
        let _ = survey_data.start_collecting(message, &inbound, &outbound, node_stats);
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

        let mut survey_data = self.survey_data.write().await;
        let _ =
            survey_data.stop_collecting(message, &inbound, &outbound, added, dropped, lost_sync);
    }
}
