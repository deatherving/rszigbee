//! A shared conformance suite for [`ZigbeeStore`] implementations.
//!
//! A trait with two implementations tested separately is a trait with two
//! subtly different meanings. Everything here is a promise the trait's callers
//! are entitled to rely on regardless of backend, so it is asserted once
//! against every backend rather than restated per implementation.
//!
//! It is public so a backend outside this crate — an `SQLite` store, a store on a
//! microcontroller's flash — can prove it behaves like the others:
//!
//! ```no_run
//! # use rszigbee_core::store::{MemoryStore, conformance};
//! # async fn example() {
//! conformance::assert_conforms(&MemoryStore::new()).await;
//! # }
//! ```
//!
//! The suite writes to the store, so it needs a **fresh, empty** one. It does
//! not test anything backend-specific: durability, atomicity, quarantine of a
//! corrupt file and permissions are properties of a particular backend and are
//! tested where they are implemented.

// This is a test suite that ships, so a backend implemented outside this crate
// can be held to the same promises. The parse-path lints exist to keep a
// malformed radio frame from taking the process down; a panic here is a failed
// assertion in a harness, which is the whole point. Same reasoning as
// `clippy.toml`'s test relaxation, which cannot apply because this is not
// `#[cfg(test)]`.
#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use crate::adapter::SecretKey;
use rszigbee_spec::ids::{EndpointId, GroupId, Ieee, Nwk};

use super::{PersistedDevice, PersistedGroup, PersistedNetwork, StoreError, ZigbeeStore};

/// Asserts that `store` honours every backend-independent promise the
/// [`ZigbeeStore`] trait makes.
///
/// # Panics
///
/// On the first violated promise, naming which one.
pub async fn assert_conforms<S: ZigbeeStore>(store: &S) {
    empty_reads_are_empty_not_errors(store).await;
    the_network_round_trips_and_is_replaced_not_appended(store).await;
    devices_are_upserted_by_address(store).await;
    groups_are_upserted_by_id(store).await;
    deletes_are_idempotent(store).await;
    backups_are_kept_not_overwritten(store).await;
    an_unknown_backup_is_not_found_rather_than_an_io_error(store).await;
    flush_succeeds(store).await;
}

/// A fresh store is empty, and reading it is not an error.
///
/// Getting this wrong turns first-ever startup into a failure, which is the
/// one run where the operator has no idea what is normal.
async fn empty_reads_are_empty_not_errors<S: ZigbeeStore>(store: &S) {
    assert_eq!(
        store
            .load_network()
            .await
            .expect("load_network on a fresh store must succeed"),
        None,
        "a fresh store must report no network, not a default one"
    );
    assert!(
        store
            .load_devices()
            .await
            .expect("load_devices on a fresh store must succeed")
            .is_empty()
    );
    assert!(
        store
            .load_groups()
            .await
            .expect("load_groups on a fresh store must succeed")
            .is_empty()
    );
    assert!(
        store
            .list_backups()
            .await
            .expect("list_backups on a fresh store must succeed")
            .is_empty()
    );
}

/// The network round-trips byte for byte, and saving twice replaces.
///
/// The frame counter is the field that matters: a store that returned a stale
/// one would silently break replay protection, so the second save has to win.
async fn the_network_round_trips_and_is_replaced_not_appended<S: ZigbeeStore>(store: &S) {
    let mut network = PersistedNetwork {
        pan_id: 0x1a62,
        // Above 2^53 on purpose: a backend serialising this as a JSON number
        // would corrupt it, and the round trip is where that shows up.
        extended_pan_id: 0x94a0_81ff_fed9_6e5c,
        channel: 11,
        nwk_update_id: 3,
        coordinator_ieee: Ieee::new(0x0017_8801_00dc_4d3f),
        key_sequence: 1,
        frame_counter: 4_294_967_000,
        // A key whose bytes are all distinct and none of them zero: a backend
        // that truncated, padded or byte-swapped it would round-trip a
        // uniform key unnoticed.
        network_key: Some(SecretKey::new([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ])),
    };
    store.save_network(&network).await.expect("save_network");
    assert_eq!(
        store.load_network().await.expect("load_network"),
        Some(network.clone()),
        "the network must round-trip exactly, including the 64-bit extended PAN id"
    );

    network.frame_counter = 4_294_967_100;
    store
        .save_network(&network)
        .await
        .expect("save_network again");
    assert_eq!(
        store
            .load_network()
            .await
            .expect("load_network")
            .map(|n| n.frame_counter),
        Some(4_294_967_100),
        "saving the network again must replace it: a stale frame counter breaks replay protection"
    );

    // And the absent case, which is a real state rather than a missing field:
    // a coordinator that declines to export its key stores `None`, and a
    // backend that turned that into an all-zero key would produce a record
    // that looks restorable and is not.
    network.network_key = None;
    store
        .save_network(&network)
        .await
        .expect("save_network without a key");
    assert_eq!(
        store
            .load_network()
            .await
            .expect("load_network")
            .map(|n| n.network_key),
        Some(None),
        "a network stored without a key must load back without one, not with a zero key"
    );
}

/// A device is keyed by IEEE address, so a second upsert updates in place.
///
/// A backend that appended would grow a duplicate every time a device was
/// heard from, and `load_devices` would report the same device twice.
async fn devices_are_upserted_by_address<S: ZigbeeStore>(store: &S) {
    let ieee = Ieee::new(0x0012_4b00_2218_9abc);
    let mut device = PersistedDevice::new(ieee, Nwk::new(0x1234));
    store.upsert_device(&device).await.expect("upsert_device");

    let other = PersistedDevice::new(Ieee::new(0x0012_4b00_2218_9abd), Nwk::new(0x5678));
    store
        .upsert_device(&other)
        .await
        .expect("upsert_device other");

    device.nwk = Nwk::new(0x4321);
    device.checkin_interval_secs = Some(300);
    store
        .upsert_device(&device)
        .await
        .expect("upsert_device again");

    let loaded = store.load_devices().await.expect("load_devices");
    assert_eq!(
        loaded.len(),
        2,
        "upsert must update in place, not append: {loaded:?}"
    );
    let found = loaded
        .iter()
        .find(|d| d.ieee == ieee)
        .expect("the upserted device must still be there");
    assert_eq!(found.nwk, Nwk::new(0x4321), "the second upsert must win");
    assert_eq!(found.checkin_interval_secs, Some(300));
}

/// Groups behave the same way, keyed by group id.
async fn groups_are_upserted_by_id<S: ZigbeeStore>(store: &S) {
    let id = GroupId(901);
    let mut group = PersistedGroup {
        id,
        members: Vec::new(),
    };
    store.upsert_group(&group).await.expect("upsert_group");

    group
        .members
        .push((Ieee::new(0x0012_4b00_2218_9abc), EndpointId(1)));
    store
        .upsert_group(&group)
        .await
        .expect("upsert_group again");

    let loaded = store.load_groups().await.expect("load_groups");
    assert_eq!(
        loaded.len(),
        1,
        "upsert must update in place, not append: {loaded:?}"
    );
    assert_eq!(loaded[0].members.len(), 1, "the second upsert must win");
}

/// Deleting something absent succeeds.
///
/// Removal runs on a leave notification, which can arrive twice or for a device
/// that was never stored. If that were an error, ordinary traffic would produce
/// failures a caller has no way to act on.
async fn deletes_are_idempotent<S: ZigbeeStore>(store: &S) {
    let ieee = Ieee::new(0x0012_4b00_2218_9abd);
    store.delete_device(ieee).await.expect("delete_device");
    store
        .delete_device(ieee)
        .await
        .expect("deleting an absent device must succeed: a duplicate leave is normal traffic");
    assert!(
        store
            .load_devices()
            .await
            .expect("load_devices")
            .iter()
            .all(|d| d.ieee != ieee),
        "a deleted device must be gone"
    );

    store
        .delete_group(GroupId(4242))
        .await
        .expect("deleting an absent group must succeed");
}

/// Every backup is kept under its own id, and the listing is newest first.
///
/// Overwriting would mean the only backup available is the most recent one,
/// which is worthless in the case backups exist for: the most recent one is
/// bad and an older one is needed.
async fn backups_are_kept_not_overwritten<S: ZigbeeStore>(store: &S) {
    let coordinator = Ieee::new(0x94a0_81ff_fed9_6e5c);
    let first = store
        .save_backup("ember", coordinator, b"first")
        .await
        .expect("save_backup");
    let second = store
        .save_backup("ember", coordinator, b"second")
        .await
        .expect("save_backup again");
    assert_ne!(
        first, second,
        "two backups taken in immediate succession must get distinct ids, or the \
         first is silently lost"
    );

    assert_eq!(
        store.load_backup(&first).await.expect("load_backup first"),
        b"first"
    );
    assert_eq!(
        store
            .load_backup(&second)
            .await
            .expect("load_backup second"),
        b"second"
    );

    let listed = store.list_backups().await.expect("list_backups");
    assert_eq!(listed.len(), 2, "both backups must be listed: {listed:?}");
    assert_eq!(listed[0].id, second, "list_backups must be newest first");
    assert_eq!(listed[0].adapter, "ember");
    assert_eq!(listed[0].coordinator_ieee, coordinator);
    assert!(
        listed[0].taken_epoch_ms >= listed[1].taken_epoch_ms,
        "the timestamp must be recorded and ordered with the listing: {listed:?}"
    );
    assert!(
        listed[1].taken_epoch_ms > 0,
        "a backup must record when it was taken, or 'restore the one from before \
         the outage' is unanswerable"
    );
}

/// Asking for a backup that is not there is [`StoreError::BackupNotFound`].
///
/// A caller distinguishes "that id is wrong" from "the disk is broken" by the
/// variant, and only one of those is worth retrying.
async fn an_unknown_backup_is_not_found_rather_than_an_io_error<S: ZigbeeStore>(store: &S) {
    match store.load_backup("no-such-backup").await {
        Err(StoreError::BackupNotFound(id)) => assert_eq!(id, "no-such-backup"),
        other => panic!("expected BackupNotFound, got {other:?}"),
    }
}

/// `flush` succeeds even when the backend buffers nothing.
async fn flush_succeeds<S: ZigbeeStore>(store: &S) {
    store
        .flush()
        .await
        .expect("flush must succeed even when there is nothing buffered");
}
