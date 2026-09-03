//! Turning a command into a frame.
//!
//! One path, deliberately: the escape hatches encode themselves, everything
//! else is lowered through the device's definition, and there is no fallback
//! for a capability the definition does not describe. A guess that is right on
//! most devices is silently wrong on the rest.

use std::time::Instant;

use rszigbee_spec::ids::{ClusterId, EndpointId, Ieee};
use tracing::warn;

use super::Task;
use crate::adapter::{AdapterError, CoordinatorAdapter};
use crate::command::{CommandError, CommandOutcome, Confirmation, DeviceCommand};
use crate::event::Event;
use crate::runtime::{definitions, encode, tuya};
use crate::store::ZigbeeStore;

impl<A: CoordinatorAdapter, S: ZigbeeStore> Task<A, S> {
    pub(super) async fn run_command(
        &mut self,
        ieee: Ieee,
        command: DeviceCommand,
    ) -> Result<CommandOutcome, CommandError> {
        // Copied out rather than held: planning a capability command needs
        // `&mut self` for the Tuya sequence counter, and a borrow of the
        // device table spanning that call would conflict.
        let Some((nwk, endpoints)) = self
            .devices
            .get(ieee)
            .map(|e| (e.info.nwk, e.info.endpoints.clone()))
        else {
            return Err(CommandError::UnknownDevice(ieee));
        };
        let started = Instant::now();

        // One counter for both ZCL and ZDO would be wrong: they are separate
        // sequence spaces on the wire.
        self.zcl_sequence = self.zcl_sequence.wrapping_add(1);
        let tsn = self.zcl_sequence;

        let (requested_endpoint, cluster, frame) =
            match command {
                DeviceCommand::Zcl(zcl) => {
                    let frame = encode::command(&self.registry, ieee, tsn, &zcl).map_err(|e| {
                        CommandError::InvalidValue {
                            capability: crate::capability::CapabilityId::from("zcl"),
                            value: e.to_string(),
                        }
                    })?;
                    (zcl.endpoint, zcl.cluster, frame)
                }
                DeviceCommand::ZclAttributes(write) => {
                    let frame = encode::attribute_write(&self.registry, ieee, tsn, &write)
                        .map_err(|e| CommandError::InvalidValue {
                            capability: crate::capability::CapabilityId::from("zcl-attributes"),
                            value: e.to_string(),
                        })?;
                    (write.endpoint, write.cluster, frame)
                }
                // Everything else is mapped from the device's definition.
                // There is deliberately no fallback: without one there is no
                // way to know which cluster a capability lives on, and a guess
                // that is right on most devices is silently wrong on the rest.
                ref other => self.plan_capability_command(ieee, other, tsn)?,
            };

        // No definition means no default endpoint to fall back on, so an
        // absent one resolves to the endpoint that actually hosts the cluster.
        let endpoint = match requested_endpoint {
            Some(id) => {
                if !endpoints.is_empty() && !endpoints.iter().any(|e| e.id == id) {
                    return Err(CommandError::UnknownEndpoint(id));
                }
                id
            }
            None => endpoints
                .iter()
                .find(|e| e.input_clusters.contains(&cluster))
                .map_or(EndpointId(1), |e| e.id),
        };
        let options = crate::adapter::TxOptions::default();

        let tx = crate::adapter::ZclTx {
            dest: crate::adapter::Destination::Unicast { ieee, nwk },
            endpoint,
            source_endpoint: EndpointId(1),
            profile: rszigbee_spec::ids::ProfileId::HA,
            cluster,
            frame,
            options,
        };

        let result = self.adapter.send_zcl(tx).await;
        let now = Instant::now();
        match result {
            Ok(response) => {
                self.record_tx(ieee, now, Ok(()));
                Ok(CommandOutcome {
                    // No definition means no converter, so there is nothing to
                    // report optimistically. Saying `None` is honest; inventing
                    // a state would be published as fact by the MQTT layer.
                    optimistic_state: None,
                    confirmed: if response.is_some() {
                        Confirmation::Acked
                    } else {
                        Confirmation::NoResponseRequested
                    },
                    latency: now.saturating_duration_since(started),
                })
            }
            Err(AdapterError::Tx(failure)) => {
                self.record_tx(ieee, now, Err(failure));
                self.emit(Event::CommandFailed {
                    ieee,
                    capability: None,
                    failure,
                });
                Err(CommandError::Delivery(failure))
            }
            Err(e) => {
                warn!(%ieee, error = %e, "command failed below the ZCL layer");
                Err(CommandError::Timeout(
                    now.saturating_duration_since(started),
                ))
            }
        }
    }

    /// Lowers a capability command through the device's definition.
    ///
    /// The Tuya datapoint table is tried first, because a definition can
    /// declare both and the datapoint is the one the device actually acts on:
    /// a Tuya switch ignores `genOnOff` entirely.
    pub(super) fn plan_capability_command(
        &mut self,
        ieee: Ieee,
        command: &DeviceCommand,
        tsn: u8,
    ) -> Result<(Option<EndpointId>, ClusterId, Vec<u8>), CommandError> {
        let definition = self.resolve(ieee).ok_or(CommandError::NoDefinition)?;

        // A behaviour is consulted only for what the table cannot say, so the
        // declarative lookup runs first.
        let behaviour_points = if tuya::command_to_datapoint(definition, command).is_none() {
            self.devices.get(ieee).and_then(|entry| {
                tuya::command_via_behavior(definition, command, &entry.info, &self.behaviors)
            })
        } else {
            None
        };
        if let Some(points) = behaviour_points {
            if points.is_empty() {
                // Claimed and refused: the behaviour decided the command was
                // invalid, and sending a partial write would leave the device
                // in a state nobody asked for.
                return Err(CommandError::InvalidValue {
                    capability: crate::capability::CapabilityId::from("behavior"),
                    value: "the device behaviour refused this command".into(),
                });
            }
            self.tuya_sequence = self.tuya_sequence.wrapping_add(1);
            let payload = rszigbee_spec::tuya::encode(self.tuya_sequence, &points);
            return Ok((
                Some(EndpointId(1)),
                rszigbee_spec::tuya::CLUSTER,
                encode::planned(tsn, rszigbee_spec::tuya::DATA_REQUEST, &payload),
            ));
        }

        if let Some(point) = tuya::command_to_datapoint(definition, command) {
            self.tuya_sequence = self.tuya_sequence.wrapping_add(1);
            let payload = rszigbee_spec::tuya::encode(self.tuya_sequence, &[point]);
            return Ok((
                Some(EndpointId(1)),
                rszigbee_spec::tuya::CLUSTER,
                encode::planned(tsn, rszigbee_spec::tuya::DATA_REQUEST, &payload),
            ));
        }

        let entry = self
            .devices
            .get(ieee)
            .ok_or(CommandError::UnknownDevice(ieee))?;
        let planned = definitions::plan_command(definition, &entry.info, command)?;
        Ok((
            Some(planned.endpoint),
            planned.cluster,
            encode::planned(tsn, planned.command, &planned.payload),
        ))
    }
}
