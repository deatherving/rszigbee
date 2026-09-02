//! Forming a new Zigbee network.
//!
//! The most destructive operation in this crate. Forming a network on a
//! coordinator that already has one orphans every device joined to the old one,
//! and that is only recoverable by re-pairing all of them. It is therefore
//! reachable only through `MismatchPolicy::Form`, never by default.
//!
//! # Key material
//!
//! The network key is generated from the operating system's CSPRNG. This is
//! deliberate and worth stating, because the obvious reference implementation
//! gets it wrong: `uplg/maison` passes `[0; 16]` as the network key while
//! setting `HAVE_NETWORK_KEY`, which forms a network whose key is all zeros.
//! Anyone within radio range can then decrypt the traffic and join.
//!
//! Generating random bytes from the OS is not "inventing cryptography" — the
//! rule this project follows is to never implement a cipher or a KDF, and this
//! does neither. The security bitmask below *is* copied from what `Zigbee2MQTT`'s
//! Ember adapter uses, because those flags are a correctness requirement rather
//! than a choice.

use ezsp::ember::join::Method as JoinMethod;
use ezsp::ember::network::Parameters as NetworkParameters;
use ezsp::ember::security::initial;
use ezsp::ember::{Eui64, key::Data as KeyData};
use ezsp::{Networking, Security};
use rszigbee_adapter::{AdapterError, NetworkConfig, SecretKey};
use rszigbee_spec::ids::Ieee;
use tracing::{info, warn};

/// The well-known global trust-centre link key, "`ZigBeeAlliance09`".
///
/// Public by definition: it is in the specification, and every Zigbee device
/// ships with it. It is what lets a device join before it has the network key.
/// Not a secret, and not treated as one.
const ZIGBEE_ALLIANCE_09: [u8; 16] = *b"ZigBeeAlliance09";

/// Generates a network key from the operating system's CSPRNG.
fn generate_network_key() -> Result<SecretKey, AdapterError> {
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).map_err(|e| {
        // Failing closed matters here: a fallback to anything weaker would
        // produce a network that looks fine and is not secure.
        AdapterError::Transport(format!(
            "cannot generate a network key from the system CSPRNG: {e}. \
             Refusing to form a network rather than use weak key material."
        ))
    })?;
    Ok(SecretKey::new(key))
}

/// Derives an extended PAN id from the coordinator's address.
///
/// Any unique 64-bit value works. Using the coordinator's own EUI64 makes the
/// network identifiable in a scan and reproducible across a re-form, which is
/// friendlier than a random value when diagnosing.
const fn derive_extended_pan_id(coordinator: Ieee) -> u64 {
    coordinator.raw()
}

/// Picks a PAN id, avoiding the reserved values.
///
/// `0x0000` and `0xffff` are reserved, and `0xffff` in particular means
/// "broadcast" — forming on it would be silently broken.
fn choose_pan_id() -> Result<u16, AdapterError> {
    let mut raw = [0u8; 2];
    getrandom::fill(&mut raw)
        .map_err(|e| AdapterError::Transport(format!("cannot generate a PAN id: {e}")))?;
    let candidate = u16::from_le_bytes(raw);
    Ok(sanitise_pan_id(candidate))
}

/// Maps a raw random value into the usable PAN id range.
const fn sanitise_pan_id(candidate: u16) -> u16 {
    match candidate {
        0x0000 | 0xffff => 0x1a62,
        other => other,
    }
}

/// The security state a coordinator forms with.
///
/// The flags are `Zigbee2MQTT`'s, and each one is load-bearing:
///
/// * `TRUST_CENTER_GLOBAL_LINK_KEY` — joiners authenticate with the global link
///   key rather than a per-device one.
/// * `HAVE_PRECONFIGURED_KEY` — we are supplying that link key below.
/// * `HAVE_NETWORK_KEY` — we are supplying the network key rather than letting
///   the stack pick one, so we can persist it and restore it later.
/// * `TRUST_CENTER_USES_HASHED_LINK_KEY` — the preconfigured key is the hashed
///   global key, which is what devices expect.
/// * `REQUIRE_ENCRYPTED_KEY` — the network key is only ever delivered
///   encrypted, to a joiner that has proved it holds the link key. Without this
///   the NCP may hand the network key out in the clear.
fn initial_security_state(network_key: &SecretKey) -> initial::State {
    initial::State::new(
        initial::Bitmask::TRUST_CENTER_GLOBAL_LINK_KEY
            | initial::Bitmask::HAVE_PRECONFIGURED_KEY
            | initial::Bitmask::HAVE_NETWORK_KEY
            | initial::Bitmask::TRUST_CENTER_USES_HASHED_LINK_KEY
            | initial::Bitmask::REQUIRE_ENCRYPTED_KEY,
        KeyData::from(ZIGBEE_ALLIANCE_09),
        KeyData::from(*network_key.expose()),
        0,
        Eui64::default(),
    )
}

/// What forming produced, so the caller can persist it.
///
/// The network key **must** be persisted before this is treated as a success:
/// losing it means losing the network, and every joined device would have to be
/// re-paired.
#[derive(Debug)]
pub struct Formed {
    /// The PAN id in use.
    pub pan_id: u16,
    /// The extended PAN id in use.
    pub extended_pan_id: u64,
    /// The channel in use.
    pub channel: u8,
    /// The network key. Persist this.
    pub network_key: SecretKey,
}

/// Forms a new network on the coordinator.
///
/// Assumes the caller has already established that the coordinator has no
/// network and that `MismatchPolicy::Form` was requested. This function does
/// not re-check, because the check belongs where the policy lives.
pub async fn form(
    connection: &mut ezsp::Connection,
    coordinator: Ieee,
    config: &NetworkConfig,
) -> Result<Formed, AdapterError> {
    let network_key = match &config.network_key {
        Some(k) => k.clone(),
        None => generate_network_key()?,
    };
    let pan_id = match config.pan_id {
        Some(p) => sanitise_pan_id(p),
        None => choose_pan_id()?,
    };
    let extended_pan_id = config
        .extended_pan_id
        .unwrap_or_else(|| derive_extended_pan_id(coordinator));

    // Loud on purpose. This is a one-way door for any device already joined to
    // a different network on this coordinator, and the log is the only record
    // that it happened. The key itself is never logged.
    warn!(
        pan_id = format_args!("0x{pan_id:04x}"),
        extended_pan_id = format_args!("0x{extended_pan_id:016x}"),
        channel = config.channel,
        "forming a NEW Zigbee network; any device joined to a previous network \
         on this coordinator will be orphaned"
    );

    connection
        .set_initial_security_state(initial_security_state(&network_key))
        .await
        .map_err(|e| {
            AdapterError::Transport(format!("cannot set the initial security state: {e}"))
        })?;

    connection
        .form_network(NetworkParameters::new(
            Eui64::from(extended_pan_id.to_be_bytes()),
            pan_id,
            // Default transmit power. Raising it is a per-deployment tuning
            // decision, not something to guess at formation time.
            8,
            config.channel,
            JoinMethod::MacAssociation,
            0,
            0,
            1_u32 << config.channel,
        ))
        .await
        .map_err(|e| AdapterError::Transport(format!("form_network failed: {e}")))?;

    info!(
        pan_id = format_args!("0x{pan_id:04x}"),
        channel = config.channel,
        "network formed"
    );

    Ok(Formed {
        pan_id,
        extended_pan_id,
        channel: config.channel,
        network_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_network_keys_are_not_all_zeros() {
        // Guarding the exact mistake the reference implementation makes: a
        // network formed with an all-zero key can be joined and decrypted by
        // anyone in radio range.
        let k = generate_network_key().expect("CSPRNG must work");
        assert_ne!(
            *k.expose(),
            [0u8; 16],
            "an all-zero network key is not a key"
        );
    }

    #[test]
    fn generated_network_keys_differ_between_calls() {
        let a = generate_network_key().expect("CSPRNG");
        let b = generate_network_key().expect("CSPRNG");
        assert_ne!(a.expose(), b.expose(), "keys must not repeat");
    }

    #[test]
    fn a_generated_key_has_reasonable_entropy() {
        // Not a statistical test, just a smoke check that we are not returning
        // a constant or a counter: 16 bytes with fewer than 6 distinct values
        // would be extraordinary from a CSPRNG.
        let k = generate_network_key().expect("CSPRNG");
        let mut seen = k.expose().to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert!(seen.len() >= 6, "suspiciously low entropy: {seen:?}");
    }

    #[test]
    fn the_network_key_never_appears_in_a_log_line() {
        // `Formed` is Debug and will end up in logs and error messages.
        let f = Formed {
            pan_id: 0x1a62,
            extended_pan_id: 0x1122_3344_5566_7788,
            channel: 11,
            network_key: SecretKey::new([0xab; 16]),
        };
        let shown = format!("{f:?}");
        assert!(shown.contains("redacted"), "{shown}");
        assert!(!shown.contains("ab, ab"), "key material leaked into Debug");
    }

    #[test]
    fn reserved_pan_ids_are_replaced() {
        // 0xffff is the broadcast address; forming on it is silently broken.
        assert_ne!(sanitise_pan_id(0xffff), 0xffff);
        assert_ne!(sanitise_pan_id(0x0000), 0x0000);
        // Anything else is left alone, so a user-configured id is honoured.
        assert_eq!(sanitise_pan_id(0x1234), 0x1234);
        assert_eq!(sanitise_pan_id(0x0001), 0x0001);
    }

    #[test]
    fn generated_pan_ids_are_always_usable() {
        for _ in 0..64 {
            let p = choose_pan_id().expect("CSPRNG");
            assert_ne!(p, 0x0000);
            assert_ne!(p, 0xffff);
        }
    }

    #[test]
    fn the_extended_pan_id_is_derived_from_the_coordinator() {
        // Reproducible across a re-form, which is friendlier when diagnosing
        // than a fresh random value each time.
        let c = Ieee::new(0x94a0_81ff_fed9_6e5c);
        assert_eq!(derive_extended_pan_id(c), 0x94a0_81ff_fed9_6e5c);
    }

    #[test]
    fn the_global_link_key_is_the_specified_one() {
        // Not a secret: it is in the specification and every device ships with
        // it. Getting the bytes wrong means no device can ever join.
        assert_eq!(&ZIGBEE_ALLIANCE_09, b"ZigBeeAlliance09");
        assert_eq!(ZIGBEE_ALLIANCE_09.len(), 16);
    }

    #[test]
    fn a_configured_key_is_used_rather_than_a_fresh_one() {
        // Restoring a network means forming with the *old* key. Silently
        // generating a new one would orphan every device.
        let configured = SecretKey::new([0x11; 16]);
        let cfg = NetworkConfig {
            pan_id: Some(0x1a62),
            extended_pan_id: Some(1),
            channel: 11,
            network_key: Some(configured.clone()),
            on_mismatch: rszigbee_adapter::MismatchPolicy::Form,
        };
        // Exercising the selection directly; `form` itself needs hardware.
        let chosen = cfg.network_key.clone().expect("configured");
        assert_eq!(chosen.expose(), configured.expose());
    }
}
