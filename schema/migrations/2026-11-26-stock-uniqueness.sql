-- 2026-11-26: two UNIQUE constraints Plex's own schema does not have
--
-- Brings a database that plex_schema.sql created before 2026-11-26 to the
-- state a fresh plex_schema.sql now creates:
--
--   * metadata_item_settings loses UNIQUE (account_id, guid). Plex declares
--     plain indexes on account_id and on guid, and the shim used to append
--     ON CONFLICT (account_id, guid) DO UPDATE to every plain INSERT, folding
--     a second row for a key into the first with view_count forced to at
--     least one. The plain index idx_metadata_item_settings_account_guid
--     stays, so the lookup by (account_id, guid) is served as before.
--   * statistics_bandwidth loses UNIQUE (account_id, device_id, timespan, at,
--     lan) and gains the two indexes Plex declares, index_..._on_at and
--     index_..._on_account_id_and_timespan_and_at. The constraint's index was
--     the only one the table had beside its primary key.
--
-- ORDER MATTERS. Apply this only to a database every Plex of which runs a
-- shim from the commit that carries this file, or later. An older shim sends
-- "ON CONFLICT (account_id, guid)" with every INSERT into
-- metadata_item_settings, and PostgreSQL refuses that outright once the
-- constraint is gone ("there is no unique or exclusion constraint matching
-- the ON CONFLICT specification"): no watch state would be written. The
-- other order is safe: the new shim on the old schema only meets the
-- constraint when Plex inserts a key twice, which is then refused (23505).
--
-- Apply, with Plex stopped (the ALTER TABLEs take ACCESS EXCLUSIVE locks):
--
--     psql -U postgres -d plex -v ON_ERROR_STOP=1 \
--          -f schema/migrations/2026-11-26-stock-uniqueness.sql
--
-- Every statement is re-runnable. Run it as postgres (CloudNativePG's
-- superuser) or as the owner of the two tables. The whole file runs in one
-- transaction: a failure leaves the database as it was.
--
-- To undo, the constraints can be added back while no key is held twice:
--     ALTER TABLE plex.metadata_item_settings
--       ADD CONSTRAINT metadata_item_settings_account_guid_unique UNIQUE (account_id, guid);
--     ALTER TABLE plex.statistics_bandwidth
--       ADD CONSTRAINT statistics_bandwidth_account_id_device_id_timespan_at_lan_key
--       UNIQUE (account_id, device_id, timespan, at, lan);
-- and an older shim needs the first of them.

SET search_path = plex, public;
SET client_min_messages = notice;
-- Do not queue behind a running Plex for ever; ON_ERROR_STOP then aborts the
-- transaction and the file can be re-applied once Plex is stopped.
SET lock_timeout = '60s';

BEGIN;

-- The indexes first, so statistics_bandwidth is never without one.
CREATE INDEX IF NOT EXISTS index_statistics_bandwidth_on_at
    ON plex.statistics_bandwidth (at);
CREATE INDEX IF NOT EXISTS index_statistics_bandwidth_on_account_id_and_timespan_and_at
    ON plex.statistics_bandwidth (account_id, timespan, at);
-- Present since the first schema; named here so that dropping the constraint
-- below can never leave the (account_id, guid) lookup unindexed.
CREATE INDEX IF NOT EXISTS idx_metadata_item_settings_account_guid
    ON plex.metadata_item_settings USING btree (account_id, guid);

ALTER TABLE plex.metadata_item_settings
    DROP CONSTRAINT IF EXISTS metadata_item_settings_account_guid_unique;
ALTER TABLE plex.statistics_bandwidth
    DROP CONSTRAINT IF EXISTS statistics_bandwidth_account_id_device_id_timespan_at_lan_key;

-- Applied as a superuser, the new indexes belong to the tables' owner already
-- (an index is owned with its table), so nothing is handed over here.

COMMIT;

ANALYZE plex.statistics_bandwidth;
