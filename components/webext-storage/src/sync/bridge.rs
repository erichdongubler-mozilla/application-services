/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use anyhow::Result;
use rusqlite::Transaction;
use std::sync::{Arc, Weak};
use sync15::bso::{IncomingBso, OutgoingBso};
use sync15::engine::{ApplyResults, BridgedEngine as Sync15BridgedEngine};
use sync_guid::Guid as SyncGuid;

use crate::db::{delete_meta, get_meta, put_meta, ThreadSafeStorageDb};
use crate::schema;
use crate::sync::incoming::{apply_actions, get_incoming, plan_incoming, stage_incoming};
use crate::sync::outgoing::{get_outgoing, record_uploaded, stage_outgoing};
use crate::WebExtStorageStore;

const LAST_SYNC_META_KEY: &str = "last_sync_time";
const SYNC_ID_META_KEY: &str = "sync_id";

impl WebExtStorageStore {
    // Returns a bridged sync engine for this store.
    pub fn bridged_engine(self: Arc<Self>) -> Arc<WebExtStorageBridgedEngine> {
        let engine = Box::new(BridgedEngine::new(&self.db));
        let bridged_engine = WebExtStorageBridgedEngine {
            bridge_impl: engine,
        };
        Arc::new(bridged_engine)
    }
}

/// A bridged engine implements all the methods needed to make the
/// `storage.sync` store work with Desktop's Sync implementation.
/// Conceptually, it's similar to `sync15::Store`, which we
/// should eventually rename and unify with this trait (#2841).
///
/// Unlike most of our other implementation which hold a strong reference
/// to the store, this engine keeps a weak reference in an attempt to keep
/// the desktop semantics as close as possible to what they were when the
/// engines all took lifetime params to ensure they don't outlive the store.
pub struct BridgedEngine {
    db: Weak<ThreadSafeStorageDb>,
}

impl BridgedEngine {
    /// Creates a bridged engine for syncing.
    pub fn new(db: &Arc<ThreadSafeStorageDb>) -> Self {
        BridgedEngine {
            db: Arc::downgrade(db),
        }
    }

    fn do_reset(&self, tx: &Transaction<'_>) -> Result<()> {
        tx.execute_batch(
            "DELETE FROM storage_sync_mirror;
             UPDATE storage_sync_data SET sync_change_counter = 1;",
        )?;
        delete_meta(tx, LAST_SYNC_META_KEY)?;
        Ok(())
    }

    fn thread_safe_storage_db(&self) -> Result<Arc<ThreadSafeStorageDb>> {
        self.db
            .upgrade()
            .ok_or_else(|| crate::error::Error::DatabaseConnectionClosed.into())
    }
}

impl Sync15BridgedEngine for BridgedEngine {
    fn last_sync(&self) -> Result<i64> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        Ok(get_meta(conn, LAST_SYNC_META_KEY)?.unwrap_or(0))
    }

    fn set_last_sync(&self, last_sync_millis: i64) -> Result<()> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        put_meta(conn, LAST_SYNC_META_KEY, &last_sync_millis)?;
        Ok(())
    }

    fn sync_id(&self) -> Result<Option<String>> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        Ok(get_meta(conn, SYNC_ID_META_KEY)?)
    }

    fn reset_sync_id(&self) -> Result<String> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        let tx = conn.unchecked_transaction()?;
        let new_id = SyncGuid::random().to_string();
        self.do_reset(&tx)?;
        put_meta(&tx, SYNC_ID_META_KEY, &new_id)?;
        tx.commit()?;
        Ok(new_id)
    }

    fn ensure_current_sync_id(&self, sync_id: &str) -> Result<String> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        let current: Option<String> = get_meta(conn, SYNC_ID_META_KEY)?;
        Ok(match current {
            Some(current) if current == sync_id => current,
            _ => {
                let conn = db.get_connection()?;
                let tx = conn.unchecked_transaction()?;
                self.do_reset(&tx)?;
                let result = sync_id.to_string();
                put_meta(&tx, SYNC_ID_META_KEY, &result)?;
                tx.commit()?;
                result
            }
        })
    }

    fn sync_started(&self) -> Result<()> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        schema::create_empty_sync_temp_tables(conn)?;
        Ok(())
    }

    fn store_incoming(&self, incoming_bsos: Vec<IncomingBso>) -> Result<()> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let signal = db.begin_interrupt_scope()?;
        let conn = db.get_connection()?;
        let tx = conn.unchecked_transaction()?;
        let incoming_content: Vec<_> = incoming_bsos
            .into_iter()
            .map(IncomingBso::into_content::<super::WebextRecord>)
            .collect();
        stage_incoming(&tx, &incoming_content, &signal)?;
        tx.commit()?;
        Ok(())
    }

    fn apply(&self) -> Result<ApplyResults> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let signal = db.begin_interrupt_scope()?;
        let conn = db.get_connection()?;
        let tx = conn.unchecked_transaction()?;
        let incoming = get_incoming(&tx)?;
        let actions = incoming
            .into_iter()
            .map(|(item, state)| (item, plan_incoming(state)))
            .collect();
        apply_actions(&tx, actions, &signal)?;
        stage_outgoing(&tx)?;
        tx.commit()?;

        Ok(get_outgoing(conn, &signal)?.into())
    }

    fn set_uploaded(&self, _server_modified_millis: i64, ids: &[SyncGuid]) -> Result<()> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        let signal = db.begin_interrupt_scope()?;
        let tx = conn.unchecked_transaction()?;
        record_uploaded(&tx, ids, &signal)?;
        tx.commit()?;

        Ok(())
    }

    fn sync_finished(&self) -> Result<()> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        schema::create_empty_sync_temp_tables(conn)?;
        Ok(())
    }

    fn reset(&self) -> Result<()> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        let tx = conn.unchecked_transaction()?;
        self.do_reset(&tx)?;
        delete_meta(&tx, SYNC_ID_META_KEY)?;
        tx.commit()?;
        Ok(())
    }

    fn wipe(&self) -> Result<()> {
        let shared_db = self.thread_safe_storage_db()?;
        let db = shared_db.lock();
        let conn = db.get_connection()?;
        let tx = conn.unchecked_transaction()?;
        // We assume the meta table is only used by sync.
        tx.execute_batch(
            "DELETE FROM storage_sync_data; DELETE FROM storage_sync_mirror; DELETE FROM meta;",
        )?;
        tx.commit()?;
        Ok(())
    }
}

pub struct WebExtStorageBridgedEngine {
    bridge_impl: Box<dyn Sync15BridgedEngine>,
}

impl WebExtStorageBridgedEngine {
    pub fn new(bridge_impl: Box<dyn Sync15BridgedEngine>) -> Self {
        Self { bridge_impl }
    }

    pub fn last_sync(&self) -> Result<i64> {
        self.bridge_impl.last_sync()
    }

    pub fn set_last_sync(&self, last_sync: i64) -> Result<()> {
        self.bridge_impl.set_last_sync(last_sync)
    }

    pub fn sync_id(&self) -> Result<Option<String>> {
        self.bridge_impl.sync_id()
    }

    pub fn reset_sync_id(&self) -> Result<String> {
        self.bridge_impl.reset_sync_id()
    }

    pub fn ensure_current_sync_id(&self, sync_id: &str) -> Result<String> {
        self.bridge_impl.ensure_current_sync_id(sync_id)
    }

    pub fn prepare_for_sync(&self, client_data: &str) -> Result<()> {
        self.bridge_impl.prepare_for_sync(client_data)
    }

    pub fn store_incoming(&self, incoming: Vec<String>) -> Result<()> {
        self.bridge_impl
            .store_incoming(self.convert_incoming_bsos(incoming)?)
    }

    pub fn apply(&self) -> Result<Vec<String>> {
        let apply_results = self.bridge_impl.apply()?;
        self.convert_outgoing_bsos(apply_results.records)
    }

    pub fn set_uploaded(&self, server_modified_millis: i64, guids: Vec<SyncGuid>) -> Result<()> {
        self.bridge_impl
            .set_uploaded(server_modified_millis, &guids)
    }

    pub fn sync_started(&self) -> Result<()> {
        self.bridge_impl.sync_started()
    }

    pub fn sync_finished(&self) -> Result<()> {
        self.bridge_impl.sync_finished()
    }

    pub fn reset(&self) -> Result<()> {
        self.bridge_impl.reset()
    }

    pub fn wipe(&self) -> Result<()> {
        self.bridge_impl.wipe()
    }

    fn convert_incoming_bsos(&self, incoming: Vec<String>) -> Result<Vec<IncomingBso>> {
        let mut bsos = Vec::with_capacity(incoming.len());
        for inc in incoming {
            bsos.push(serde_json::from_str::<IncomingBso>(&inc)?);
        }
        Ok(bsos)
    }

    // Encode OutgoingBso's into JSON for UniFFI
    fn convert_outgoing_bsos(&self, outgoing: Vec<OutgoingBso>) -> Result<Vec<String>> {
        let mut bsos = Vec::with_capacity(outgoing.len());
        for e in outgoing {
            bsos.push(serde_json::to_string(&e)?);
        }
        Ok(bsos)
    }
}

impl From<anyhow::Error> for crate::error::Error {
    fn from(value: anyhow::Error) -> Self {
        crate::error::Error::SyncError(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test::new_mem_thread_safe_storage_db;
    use crate::db::StorageDb;
    use sync15::engine::BridgedEngine;

    fn query_count(db: &StorageDb, table: &str) -> u32 {
        let conn = db.get_connection().expect("should retrieve connection");
        conn.query_row_and_then(&format!("SELECT COUNT(*) FROM {};", table), [], |row| {
            row.get::<_, u32>(0)
        })
        .expect("should work")
    }

    // Sets up mock data for the tests here.
    fn setup_mock_data(engine: &super::BridgedEngine) -> Result<()> {
        {
            let shared = engine.thread_safe_storage_db()?;
            let db = shared.lock();
            let conn = db.get_connection().expect("should retrieve connection");
            conn.execute(
                "INSERT INTO storage_sync_data (ext_id, data, sync_change_counter)
                    VALUES ('ext-a', 'invalid-json', 2)",
                [],
            )?;
            conn.execute(
                "INSERT INTO storage_sync_mirror (guid, ext_id, data)
                    VALUES ('guid', 'ext-a', '3')",
                [],
            )?;
        }
        engine.set_last_sync(1)?;

        let shared = engine.thread_safe_storage_db()?;
        let db = shared.lock();
        // and assert we wrote what we think we did.
        assert_eq!(query_count(&db, "storage_sync_data"), 1);
        assert_eq!(query_count(&db, "storage_sync_mirror"), 1);
        assert_eq!(query_count(&db, "meta"), 1);
        Ok(())
    }

    // Assuming a DB setup with setup_mock_data, assert it was correctly reset.
    fn assert_reset(engine: &super::BridgedEngine) -> Result<()> {
        // A reset never wipes data...
        let shared = engine.thread_safe_storage_db()?;
        let db = shared.lock();
        let conn = db.get_connection().expect("should retrieve connection");
        assert_eq!(query_count(&db, "storage_sync_data"), 1);

        // But did reset the change counter.
        let cc = conn.query_row_and_then(
            "SELECT sync_change_counter FROM storage_sync_data WHERE ext_id = 'ext-a';",
            [],
            |row| row.get::<_, u32>(0),
        )?;
        assert_eq!(cc, 1);
        // But did wipe the mirror...
        assert_eq!(query_count(&db, "storage_sync_mirror"), 0);
        // And the last_sync should have been wiped.
        assert!(get_meta::<i64>(conn, LAST_SYNC_META_KEY)?.is_none());
        Ok(())
    }

    // Assuming a DB setup with setup_mock_data, assert it has not been reset.
    fn assert_not_reset(engine: &super::BridgedEngine) -> Result<()> {
        let shared = engine.thread_safe_storage_db()?;
        let db = shared.lock();
        let conn = db.get_connection().expect("should retrieve connection");
        assert_eq!(query_count(&db, "storage_sync_data"), 1);
        let cc = conn.query_row_and_then(
            "SELECT sync_change_counter FROM storage_sync_data WHERE ext_id = 'ext-a';",
            [],
            |row| row.get::<_, u32>(0),
        )?;
        assert_eq!(cc, 2);
        assert_eq!(query_count(&db, "storage_sync_mirror"), 1);
        // And the last_sync should remain.
        assert!(get_meta::<i64>(conn, LAST_SYNC_META_KEY)?.is_some());
        Ok(())
    }

    #[test]
    fn test_wipe() -> Result<()> {
        let strong = new_mem_thread_safe_storage_db();
        let engine = super::BridgedEngine::new(&strong);

        setup_mock_data(&engine)?;

        engine.wipe()?;

        let shared = engine.thread_safe_storage_db()?;
        let db = shared.lock();

        assert_eq!(query_count(&db, "storage_sync_data"), 0);
        assert_eq!(query_count(&db, "storage_sync_mirror"), 0);
        assert_eq!(query_count(&db, "meta"), 0);
        Ok(())
    }

    #[test]
    fn test_reset() -> Result<()> {
        let strong = &new_mem_thread_safe_storage_db();
        let engine = super::BridgedEngine::new(strong);

        setup_mock_data(&engine)?;
        {
            let db = strong.lock();
            let conn = db.get_connection()?;
            put_meta(conn, SYNC_ID_META_KEY, &"sync-id".to_string())?;
        }

        engine.reset()?;
        assert_reset(&engine)?;

        {
            let db = strong.lock();
            let conn = db.get_connection()?;
            // Only an explicit reset kills the sync-id, so check that here.
            assert_eq!(get_meta::<String>(conn, SYNC_ID_META_KEY)?, None);
        }

        Ok(())
    }

    #[test]
    fn test_ensure_missing_sync_id() -> Result<()> {
        let strong = new_mem_thread_safe_storage_db();
        let engine = super::BridgedEngine::new(&strong);

        setup_mock_data(&engine)?;

        assert_eq!(engine.sync_id()?, None);
        // We don't have a sync ID - so setting one should reset.
        engine.ensure_current_sync_id("new-id")?;
        // should have cause a reset.
        assert_reset(&engine)?;
        Ok(())
    }

    #[test]
    fn test_ensure_new_sync_id() -> Result<()> {
        let strong = new_mem_thread_safe_storage_db();
        let engine = super::BridgedEngine::new(&strong);

        setup_mock_data(&engine)?;

        {
            let storage_db = &engine.thread_safe_storage_db()?;
            let db = storage_db.lock();
            let conn = db.get_connection()?;
            put_meta(conn, SYNC_ID_META_KEY, &"old-id".to_string())?;
        }

        assert_not_reset(&engine)?;
        assert_eq!(engine.sync_id()?, Some("old-id".to_string()));

        engine.ensure_current_sync_id("new-id")?;
        // should have cause a reset.
        assert_reset(&engine)?;
        // should have the new id.
        assert_eq!(engine.sync_id()?, Some("new-id".to_string()));
        Ok(())
    }

    #[test]
    fn test_ensure_same_sync_id() -> Result<()> {
        let strong = new_mem_thread_safe_storage_db();
        let engine = super::BridgedEngine::new(&strong);

        setup_mock_data(&engine)?;
        assert_not_reset(&engine)?;

        {
            let storage_db = &engine.thread_safe_storage_db()?;
            let db = storage_db.lock();
            let conn = db.get_connection()?;
            put_meta(conn, SYNC_ID_META_KEY, &"sync-id".to_string())?;
        }

        engine.ensure_current_sync_id("sync-id")?;
        // should not have reset.
        assert_not_reset(&engine)?;
        Ok(())
    }

    #[test]
    fn test_reset_sync_id() -> Result<()> {
        let strong = new_mem_thread_safe_storage_db();
        let engine = super::BridgedEngine::new(&strong);

        setup_mock_data(&engine)?;

        {
            let storage_db = &engine.thread_safe_storage_db()?;
            let db = storage_db.lock();
            let conn = db.get_connection()?;
            put_meta(conn, SYNC_ID_META_KEY, &"sync-id".to_string())?;
        }

        assert_eq!(engine.sync_id()?, Some("sync-id".to_string()));
        let new_id = engine.reset_sync_id()?;
        // should have cause a reset.
        assert_reset(&engine)?;
        assert_eq!(engine.sync_id()?, Some(new_id));
        Ok(())
    }
}
