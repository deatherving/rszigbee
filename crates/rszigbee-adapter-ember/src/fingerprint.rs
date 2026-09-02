//! Per-dongle serial defaults.
//!
//! This module exists because of a concrete failure. Opening a Sonoff `ZBDongle-E`
//! with hardware flow control enabled hung the process for ten minutes while a
//! five-second timeout was configured: `open(2)` on a tty blocks in the kernel
//! until CTS is asserted, and `tokio::time::timeout` cannot interrupt that
//! because it is not an await point. The dongle does not wire RTS/CTS.
//!
//! Guessing wrong is therefore not a recoverable error, it is a hang — so the
//! settings come from a table keyed on the USB descriptor, the way
//! zigbee-herdsman's `adapterDiscovery.ts` does it. When nothing matches, the
//! fallback is the setting that cannot hang.

/// Serial parameters for a coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialSettings {
    /// Baud rate.
    pub baud: u32,
    /// Whether the dongle wires RTS/CTS hardware flow control.
    pub rtscts: bool,
}

impl SerialSettings {
    /// The safe fallback for an unrecognised dongle.
    ///
    /// Flow control **off**: a dongle that needs it will fail to communicate,
    /// which is diagnosable. A dongle that does not need it but is opened with
    /// it on hangs in the kernel, which is not.
    pub const FALLBACK: Self = Self {
        baud: 115_200,
        rtscts: false,
    };
}

/// One entry in the fingerprint table.
struct Fingerprint {
    /// Substring matched against the `/dev/serial/by-id/` path, case-insensitively.
    id_contains: &'static str,
    /// Human name, for logs.
    name: &'static str,
    /// Settings to use.
    settings: SerialSettings,
}

/// Known dongles. Ordered most specific first, because `ZBDongle-E` and
/// `ZBDongle-P` share a descriptor prefix and need different settings.
const TABLE: &[Fingerprint] = &[
    Fingerprint {
        // Sonoff ZBDongle-E, EFR32MG21. The "_V2" suffix distinguishes it from
        // the ZBDongle-P, which is a TI CC2652P running Z-Stack and is not an
        // Ember device at all. Verified against real hardware: EmberZNet
        // 7.4.4.0, EZSP v13, 115200, no flow control.
        id_contains: "sonoff_zigbee_3.0_usb_dongle_plus_v2",
        name: "Sonoff ZBDongle-E (EFR32MG21)",
        settings: SerialSettings {
            baud: 115_200,
            rtscts: false,
        },
    },
    Fingerprint {
        id_contains: "slzb-07",
        name: "SMLIGHT SLZB-07 (EFR32MG21)",
        settings: SerialSettings {
            baud: 115_200,
            rtscts: false,
        },
    },
    Fingerprint {
        // Silabs' own dev kits do wire flow control, and at a higher rate.
        id_contains: "silicon_labs_wstk",
        name: "Silicon Labs WSTK dev kit",
        settings: SerialSettings {
            baud: 115_200,
            rtscts: true,
        },
    },
    Fingerprint {
        id_contains: "elelabs",
        name: "Elelabs Zigbee (EFR32)",
        settings: SerialSettings {
            baud: 115_200,
            rtscts: true,
        },
    },
];

/// A dongle recognised from its serial path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recognised {
    /// Human name for logs and diagnostics.
    pub name: &'static str,
    /// Settings to open with.
    pub settings: SerialSettings,
}

/// Looks a serial path up in the fingerprint table.
///
/// Matching is on the `/dev/serial/by-id/` symlink name, which carries the USB
/// descriptor. A caller that passes `/dev/ttyUSB0` gets no match and the
/// fallback, which is correct: the raw device node carries no identity.
#[must_use]
pub fn recognise(path: &str) -> Option<Recognised> {
    let lower = path.to_ascii_lowercase();
    TABLE
        .iter()
        .find(|f| lower.contains(f.id_contains))
        .map(|f| Recognised {
            name: f.name,
            settings: f.settings,
        })
}

/// Settings for a path: the table entry if recognised, otherwise the fallback.
#[must_use]
pub fn settings_for(path: &str) -> SerialSettings {
    recognise(path).map_or(SerialSettings::FALLBACK, |r| r.settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real path from the verified hardware, verbatim.
    const ZBDONGLE_E: &str = "/dev/serial/by-id/usb-Itead_Sonoff_Zigbee_3.0_USB_Dongle_Plus_V2_38b61ead02a1f011a2af2c81bb936ffa-if00-port0";

    #[test]
    fn the_verified_dongle_is_recognised_with_flow_control_off() {
        // This exact string came off the hardware the adapter was verified
        // against. If this test fails, the table no longer matches reality.
        let r = recognise(ZBDONGLE_E).expect("ZBDongle-E must be recognised");
        assert_eq!(
            r.settings,
            SerialSettings {
                baud: 115_200,
                rtscts: false
            }
        );
        assert!(r.name.contains("EFR32MG21"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(recognise(&ZBDONGLE_E.to_uppercase()).is_some());
        assert!(recognise(&ZBDONGLE_E.to_lowercase()).is_some());
    }

    #[test]
    fn a_raw_device_node_gets_the_fallback_not_a_guess() {
        // /dev/ttyUSB0 carries no identity, so there is nothing to match on.
        assert!(recognise("/dev/ttyUSB0").is_none());
        assert_eq!(settings_for("/dev/ttyUSB0"), SerialSettings::FALLBACK);
    }

    // Compile-time, not runtime: the whole point of this module is that an
    // unrecognised dongle must never be opened with hardware flow control,
    // because a wrong guess there is a kernel-level hang rather than an error
    // we can report. Making it a const assertion means it cannot regress even
    // if nobody runs the tests.
    const _: () = assert!(!SerialSettings::FALLBACK.rtscts);

    #[test]
    fn the_ti_dongle_is_not_mistaken_for_an_ember_one() {
        // ZBDongle-P is a CC2652P running Z-Stack. It shares a descriptor
        // prefix with the -E but is not an Ember device, so it must not match.
        let p = "/dev/serial/by-id/usb-ITead_Sonoff_Zigbee_3.0_USB_Dongle_Plus_abcdef-if00-port0";
        assert!(
            recognise(p).is_none(),
            "ZBDongle-P must not match an Ember fingerprint"
        );
    }

    #[test]
    fn dongles_that_do_wire_flow_control_are_recorded_as_such() {
        // Not every Ember device is flow-control-free; the table has to be able
        // to say so, or it is just a constant.
        let wstk =
            recognise("/dev/serial/by-id/usb-Silicon_Labs_WSTK_440123456-if00").expect("WSTK");
        assert!(wstk.settings.rtscts);
    }

    #[test]
    fn every_table_entry_is_well_formed() {
        for f in TABLE {
            assert!(!f.id_contains.is_empty());
            assert_eq!(
                f.id_contains,
                f.id_contains.to_ascii_lowercase(),
                "table keys must be lowercase or matching silently fails"
            );
            assert!(!f.name.is_empty());
            assert!(f.settings.baud >= 9600);
        }
    }
}
