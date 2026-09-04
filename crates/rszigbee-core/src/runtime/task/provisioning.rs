//! Bringing a device into service.
//!
//! Joining, interviewing, and configuring — the sequence that turns a device
//! that has appeared on the network into one that reports. The configure step
//! is the half most easily forgotten: a device that resolves a definition and
//! is never bound is recognised and permanently silent.

use std::time::{Duration, Instant};

use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::Task;
use crate::adapter::CoordinatorAdapter;
use crate::device::InterviewState;
use crate::event::Event;
use crate::runtime::{
    ConfigureOutcome, InterviewOutcome, InterviewUpdate, RuntimeError, definitions, encode,
    interview,
};
use crate::store::{FRAME_COUNTER_MARGIN, PersistedNetwork, ZigbeeStore};

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    /// Records network identity, including what it takes to restore it.
    ///
    /// Both of the fields that used to be placeholders are now read from the
    /// coordinator. That matters more than completeness:
    ///
    /// * The **frame counter** is what stops a restarted coordinator from
    ///   having its frames discarded as replays. It was stored as `0`, which is
    ///   the one value guaranteed to be wrong.
    /// * The **network key** is what makes the record a backup rather than a
    ///   description. The comment here used to say a coordinator would not hand
    ///   it back; `EmberZNet` does, and the adapter now asks.
    ///
    /// Called on start and again on shutdown. See
    /// [`refresh_frame_counter`](Self::refresh_frame_counter) for why once is
    /// not enough.
    pub(super) async fn persist_network_if_needed(&mut self) {
        if self.network_known {
            return;
        }
        let Ok(info) = self.adapter.network_info().await else {
            debug!("no network parameters available to persist yet");
            return;
        };
        // Fetched separately, and only here, because this is the one place it
        // is about to be written. A key that travels with the parameters ends
        // up in a log line eventually.
        let network_key = match self.adapter.network_key().await {
            Ok(key) => key,
            Err(e) => {
                warn!(error = %e, "could not read the network key; storing the network without it");
                None
            }
        };
        if network_key.is_none() {
            warn!(
                "this coordinator did not export its network key, so the stored \
                 network describes the network but cannot recreate it on \
                 replacement hardware"
            );
        }
        let counter = info.frame_counter.saturating_add(FRAME_COUNTER_MARGIN);
        let record = PersistedNetwork {
            pan_id: info.pan_id,
            extended_pan_id: info.extended_pan_id,
            channel: info.channel,
            nwk_update_id: info.nwk_update_id,
            coordinator_ieee: self.coordinator,
            key_sequence: info.key_sequence,
            frame_counter: counter,
            network_key,
        };
        if let Err(e) = self.store.save_network(&record).await {
            warn!(error = %e, "could not persist network identity");
        } else {
            self.network_known = true;
            self.persisted_frame_counter = counter;
        }
    }

    /// Re-stores the frame counter once the live one has caught up.
    ///
    /// [`FRAME_COUNTER_MARGIN`] buys a bounded number of frames, not an
    /// unbounded one. A long-running coordinator transmits past the stored
    /// value, and from that moment a crash would roll the counter back and its
    /// frames would be dropped as replays by every device that remembers the
    /// higher value. So the margin is topped up when it is consumed -- which is
    /// a write every thousand-odd frames, not one per frame.
    pub(super) async fn refresh_frame_counter(&mut self) {
        if !self.network_known {
            return;
        }
        let Ok(info) = self.adapter.network_info().await else {
            return;
        };
        if info.frame_counter < self.persisted_frame_counter {
            return;
        }
        let Ok(Some(mut record)) = self.store.load_network().await else {
            return;
        };
        let counter = info.frame_counter.saturating_add(FRAME_COUNTER_MARGIN);
        record.frame_counter = counter;
        if let Err(e) = self.store.save_network(&record).await {
            warn!(error = %e, "could not re-store the frame counter");
        } else {
            debug!(counter, "frame counter margin topped up");
            self.persisted_frame_counter = counter;
        }
    }

    pub(super) async fn permit_join(
        &mut self,
        duration: Duration,
        via: Option<Ieee>,
    ) -> Result<(), RuntimeError> {
        // A router is named by its permanent address at this layer and by its
        // short address at the adapter, so the translation happens here.
        let nwk = match via {
            Some(ieee) => Some(
                self.devices
                    .get(ieee)
                    .map(|e| e.info.nwk)
                    .ok_or(RuntimeError::UnknownDevice(ieee))?,
            ),
            None => None,
        };
        self.adapter.permit_join(duration, nwk).await?;
        let permitted = !duration.is_zero();
        self.emit(Event::PermitJoinChanged {
            permitted,
            until: permitted.then(|| Instant::now() + duration),
            via,
        });
        Ok(())
    }

    /// Binds and configures reporting, so the device actually sends data.
    ///
    /// Runs after resolution, because the plan comes from the definition. Both
    /// halves are needed and in this order: a binding tells the device where
    /// to send reports, and configuring reporting tells it when. Configure
    /// without bind and the device generates reports with nowhere to send
    /// them; bind without configure and many devices report only on a poll.
    ///
    /// A failure on one step is logged and the rest continue. Devices refuse
    /// bindings and reporting configuration routinely -- some because they
    /// report unconditionally and consider it meaningless -- and abandoning
    /// the whole plan on the first refusal would leave later, working steps
    /// unconfigured.
    pub(super) async fn execute_configure_plan(&mut self, ieee: Ieee) -> ConfigureOutcome {
        let Some(definition) = self.resolve(ieee) else {
            return ConfigureOutcome::default();
        };
        let Some(entry) = self.devices.get(ieee) else {
            return ConfigureOutcome::default();
        };
        let plan = definitions::configure_plan(definition, &entry.info);
        if plan.is_empty() {
            return ConfigureOutcome::default();
        }

        let device_nwk = entry.info.nwk;
        let coordinator = self.coordinator;
        let mut bound: std::collections::HashSet<(EndpointId, ClusterId)> =
            std::collections::HashSet::new();
        let mut outcome = ConfigureOutcome::default();

        for step in plan {
            // One binding per (endpoint, cluster), however many attributes on
            // it are being configured.
            if bound.insert((step.endpoint, step.cluster)) {
                match self
                    .bind(ieee, device_nwk, step.endpoint, step.cluster, coordinator)
                    .await
                {
                    Ok(()) => {
                        debug!(%ieee, cluster = step.cluster.0, "bound");
                        outcome.bound = outcome.bound.saturating_add(1);
                    }
                    Err(e) => {
                        warn!(%ieee, cluster = step.cluster.0, error = %e, "bind failed");
                        outcome.failed = outcome.failed.saturating_add(1);
                    }
                }
            }

            let Some(attribute) = step.attribute else {
                continue;
            };
            // The wire type decides whether a reportable change is sent at
            // all, so a wrong type produces a frame the device rejects. The
            // plan carries it; the registry is only a fallback, because it
            // does not know every cluster a definition can name.
            let ty = step.attribute_type.or_else(|| {
                self.registry
                    .attr(Some(ieee), step.cluster, attribute)
                    .map(|a| a.ty)
            });
            let Some(ty) = ty else {
                warn!(
                    %ieee,
                    cluster = step.cluster.0,
                    attribute = attribute.0,
                    "no wire type known, so reporting cannot be configured safely"
                );
                outcome.failed = outcome.failed.saturating_add(1);
                continue;
            };

            match self
                .configure_reporting(
                    ieee,
                    device_nwk,
                    step.endpoint,
                    step.cluster,
                    &[encode::ReportRecord {
                        attribute,
                        ty,
                        min_interval: step.min_interval,
                        max_interval: step.max_interval,
                        min_change: step.min_change,
                    }],
                )
                .await
            {
                Ok(()) => outcome.configured = outcome.configured.saturating_add(1),
                Err(e) => {
                    warn!(
                        %ieee,
                        cluster = step.cluster.0,
                        attribute = attribute.0,
                        error = %e,
                        "configuring reporting failed"
                    );
                    outcome.failed = outcome.failed.saturating_add(1);
                }
            }
        }

        info!(
            %ieee,
            bound = outcome.bound,
            configured = outcome.configured,
            failed = outcome.failed,
            "reporting configured"
        );
        outcome
    }

    /// Applies one interview update. The only place interview results are
    /// written, and it runs on the loop.
    pub(super) async fn apply_interview_update(&mut self, ieee: Ieee, update: InterviewUpdate) {
        match update {
            InterviewUpdate::Started => {
                if let Some(entry) = self.devices.get_mut(ieee) {
                    entry.info.interview = InterviewState::InProgress;
                }
                self.emit(Event::InterviewStarted { ieee });
                // Persisted immediately so a crash mid-interview resumes as
                // `InProgress` rather than looking like it never began.
                self.persist(ieee).await;
            }
            InterviewUpdate::NodeDescriptor {
                kind,
                power,
                sleepy,
            } => {
                if let Some(entry) = self.devices.get_mut(ieee) {
                    entry.info.kind = kind;
                    entry.info.power_source = power;
                    // The one fact that decides whether this device can ever be
                    // probed on demand. Getting it wrong means either probing a
                    // sleeping device forever or never noticing a dead one.
                    entry.reachability.is_sleepy = sleepy;
                }
                self.persist(ieee).await;
            }
            InterviewUpdate::Step(step) => {
                self.emit(Event::InterviewProgress { ieee, step });
            }
            InterviewUpdate::Finished(outcome) => {
                if let Some(entry) = self.devices.get_mut(ieee) {
                    entry.info.interview = outcome.state;
                    if !outcome.endpoints.is_empty() {
                        entry.info.endpoints.clone_from(&outcome.endpoints);
                        entry.info.endpoints.sort_by_key(|e| e.id);
                    }
                    // The model string, which is what a definition matches on.
                    if let Some(basic) = &outcome.basic {
                        entry.info.basic = basic.clone();
                    }
                }
                self.persist(ieee).await;
                self.emit(Event::InterviewFinished {
                    ieee,
                    state: outcome.state,
                });

                // Resolution happens here rather than mid-interview because it
                // needs the model *and* the endpoints: some fingerprints match
                // on the endpoint layout.
                let resolved = self
                    .resolve(ieee)
                    .map(|d| (d.model.clone(), d.is_complete()));
                if let Some((model, complete)) = &resolved
                    && !complete
                {
                    debug!(
                        %ieee,
                        model,
                        "definition matched but is incomplete: some behaviour is not expressed"
                    );
                }
                let matched = resolved.is_some();
                // Emitted either way. An unrecognised device still produces raw
                // events, and this is the signal that what it needs is a
                // definition.
                self.emit(Event::DefinitionResolved {
                    ieee,
                    model: resolved.map(|(model, _)| model),
                    source: crate::event::DefinitionSource::Bundled,
                });

                if matched {
                    // Before anything else: a frame from a custom cluster
                    // cannot be decoded until its types are known.
                    self.register_custom_clusters(ieee);
                    self.apply_definition_metadata(ieee).await;
                }

                // Resolving a definition is only half of making a sensor work.
                // The other half is binding and configuring reporting, without
                // which the device is recognised and permanently silent.
                if matched {
                    let _ = self.execute_configure_plan(ieee).await;
                }
            }
        }
    }

    pub(super) fn spawn_interview(
        &mut self,
        ieee: Ieee,
        reply: Option<oneshot::Sender<Result<InterviewOutcome, RuntimeError>>>,
    ) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            let result = interview::run(&handle, ieee).await;
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        });
    }
}
