use super::*;

// ═════════════════════════════════════════════════════════════════════════
// Db-to-Pool Mapping
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn db_to_pool_assign_and_find() {
    let dtp = DbToPool::new();
    dtp.assign(0x100, 5);
    assert_eq!(dtp.find(0x100), Some(5));
}

#[test]
fn db_to_pool_find_missing_returns_none() {
    let dtp = DbToPool::new();
    assert_eq!(dtp.find(0x100), None);
}

#[test]
fn db_to_pool_release_removes() {
    let dtp = DbToPool::new();
    dtp.assign(0x100, 5);
    assert_eq!(dtp.release(0x100), Some(5));
    assert_eq!(dtp.find(0x100), None);
}

#[test]
fn db_to_pool_multiple_handles_same_slot() {
    // Multiple sqlite3* handles can share a pool slot
    let dtp = DbToPool::new();
    dtp.assign(0x100, 5);
    dtp.assign(0x200, 5);
    assert_eq!(dtp.find(0x100), Some(5));
    assert_eq!(dtp.find(0x200), Some(5));
}

#[test]
fn db_to_pool_counts_handles_still_holding_a_slot() {
    // This is what the zombie reclaim needs and did not have: whether anyone
    // still refers to a slot. A handle Plex has not closed is a reference,
    // however long the slot has been idle and whatever became of the thread
    // that opened it.
    let dtp = DbToPool::new();
    assert_eq!(dtp.references(5), 0);

    dtp.assign(0x100, 5);
    dtp.assign(0x200, 5);
    dtp.assign(0x300, 7);
    assert_eq!(dtp.references(5), 2, "two handles are holding slot 5");
    assert_eq!(dtp.references(7), 1);

    dtp.release(0x100);
    assert_eq!(dtp.references(5), 1, "closing one handle leaves the other");

    dtp.release(0x200);
    assert_eq!(
        dtp.references(5),
        0,
        "the last handle closing frees the slot"
    );
}

#[test]
fn db_to_pool_clear() {
    let dtp = DbToPool::new();
    dtp.assign(0x100, 5);
    dtp.assign(0x200, 7);
    dtp.clear();
    assert_eq!(dtp.len(), 0);
}
