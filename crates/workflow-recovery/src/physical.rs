//! Embedded physical `DurableStore` adapter backed by redb.
//!
//! The database stores one versioned postcard snapshot per run. redb owns the
//! physical transaction, file locking, page checksums, and crash-safe commit;
//! this module owns the durable-state schema and maps backend failures to the
//! public `StoreError` categories.

use crate::durable::{
    CommitRequest, CommitResult, DurableRunState, DurableStore, StoreError, StoreRevision,
    apply_mutation,
};
use redb::{Database, DatabaseError, ReadableTable, TableDefinition};
use std::fmt::Display;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RUNS: TableDefinition<&str, &[u8]> = TableDefinition::new("kernis_runs_v1");
const FORMAT_MAGIC: &[u8] = b"KERNIS-DURABLE-STATE";
const FORMAT_VERSION: u16 = 3;
const CHECKSUM_LEN: usize = std::mem::size_of::<u64>();
const DATABASE_OPEN_RETRIES: usize = 40;
const DATABASE_OPEN_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Physical durable store using one embedded redb file.
///
/// The store handle is a logical connection to the path. Database handles are
/// opened for individual operations so multiple `FileDurableStore` values can
/// observe the same file and exercise revision CAS semantics. redb serializes
/// physical writers and rejects a currently unavailable file as
/// [`StoreError::BackendUnavailable`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDurableStore {
    path: PathBuf,
}

impl FileDurableStore {
    /// Opens or creates a physical durable store at `path`.
    ///
    /// A newly created file receives the current table schema. Existing files
    /// with an incompatible redb format or missing Kernis table fail closed
    /// with an explicit backend or corruption category.
    ///
    /// # Errors
    ///
    /// Returns a typed [`StoreError`] when the backend cannot be opened or its
    /// persisted schema is not trusted.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(map_io_error)?;
        }

        let store = Self { path };
        let existed = store.path.exists();
        let (database, created_by_store) = if existed {
            (open_existing_database(&store.path)?, false)
        } else {
            match Database::create(&store.path) {
                Ok(database) => (database, true),
                Err(DatabaseError::DatabaseAlreadyOpen) => {
                    (open_existing_database(&store.path)?, false)
                }
                Err(error) => return Err(map_database_error(error)),
            }
        };

        if database_has_table(&database)? {
            return Ok(store);
        }
        if created_by_store {
            initialize_table(&database)?;
            return Ok(store);
        }
        drop(database);
        if wait_for_initialized_table(&store.path)? {
            return Ok(store);
        }
        Err(StoreError::DataCorruption(
            "physical store is missing the Kernis durable-state table".to_string(),
        ))
    }

    /// Returns the database path used by this logical connection.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn database(&self) -> Result<Database, StoreError> {
        open_existing_database(&self.path)
    }
}

impl DurableStore for FileDurableStore {
    fn create_run(&mut self, run_id: crate::RunId) -> Result<StoreRevision, StoreError> {
        let database = self.database()?;
        let state = DurableRunState::new(run_id.clone());
        let encoded = encode_state(&state)?;
        let write = database.begin_write().map_err(map_transaction_error)?;
        let result = {
            let mut table = write.open_table(RUNS).map_err(map_table_error)?;
            if table
                .get(run_id.as_str())
                .map_err(map_storage_error)?
                .is_some()
            {
                Err(StoreError::RunAlreadyExists(run_id))
            } else {
                table
                    .insert(run_id.as_str(), encoded.as_slice())
                    .map_err(map_storage_error)?;
                Ok(StoreRevision::INITIAL)
            }
        };

        let revision = result?;
        write.commit().map_err(map_commit_error)?;
        Ok(revision)
    }

    fn load_run(&self, run_id: &crate::RunId) -> Result<DurableRunState, StoreError> {
        let database = self.database()?;
        let read = database.begin_read().map_err(map_transaction_error)?;
        let table = read.open_table(RUNS).map_err(map_table_error)?;
        let encoded = table
            .get(run_id.as_str())
            .map_err(map_storage_error)?
            .map(|value| value.value().to_vec())
            .ok_or_else(|| StoreError::RunNotFound(run_id.clone()))?;
        let state = decode_state(&encoded)?;
        if state.run_id() != run_id {
            return Err(StoreError::DataCorruption(format!(
                "run row key {} contains state for {}",
                run_id,
                state.run_id()
            )));
        }
        Ok(state)
    }

    fn commit(&mut self, request: CommitRequest) -> Result<CommitResult, StoreError> {
        if request.mutations.is_empty() {
            return Err(StoreError::EmptyCommit);
        }

        let database = self.database()?;
        let write = database.begin_write().map_err(map_transaction_error)?;
        let result = {
            let mut table = write.open_table(RUNS).map_err(map_table_error)?;
            let encoded = table
                .get(request.run_id.as_str())
                .map_err(map_storage_error)?
                .map(|value| value.value().to_vec())
                .ok_or_else(|| StoreError::RunNotFound(request.run_id.clone()))?;
            let state = decode_state(&encoded)?;
            if state.run_id() != &request.run_id {
                return Err(StoreError::DataCorruption(format!(
                    "run row key {} contains state for {}",
                    request.run_id,
                    state.run_id()
                )));
            }

            if let Some((existing, _revision)) = state.idempotency.get(&request.idempotency_key) {
                if existing == &request.mutations {
                    return Ok(CommitResult {
                        revision: state.revision(),
                        replayed: true,
                    });
                }
                return Err(StoreError::IdempotencyConflict {
                    key: request.idempotency_key.clone(),
                });
            }
            if request.expected_revision != state.revision() {
                return Err(StoreError::RevisionConflict {
                    expected: request.expected_revision,
                    actual: state.revision(),
                });
            }

            let revision =
                state
                    .revision()
                    .checked_next()
                    .ok_or(StoreError::RevisionExhausted {
                        current: state.revision(),
                    })?;
            let mut candidate = state;
            for mutation in &request.mutations {
                apply_mutation(&mut candidate, mutation)?;
            }
            candidate.revision = revision;
            candidate.record_commit(
                request.idempotency_key.clone(),
                request.mutations.clone(),
                revision,
            );
            let encoded = encode_state(&candidate)?;
            table
                .insert(request.run_id.as_str(), encoded.as_slice())
                .map_err(map_storage_error)?;
            Ok(CommitResult {
                revision,
                replayed: false,
            })
        };

        let result = result?;
        if !result.replayed {
            write.commit().map_err(map_commit_error)?;
        }
        Ok(result)
    }
}

fn database_has_table(database: &Database) -> Result<bool, StoreError> {
    let read = database.begin_read().map_err(map_transaction_error)?;
    match read.open_table(RUNS) {
        Ok(_table) => Ok(true),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(error) => Err(map_table_error(error)),
    }
}

fn open_existing_database(path: &Path) -> Result<Database, StoreError> {
    for attempt in 0..=DATABASE_OPEN_RETRIES {
        match Database::open(path) {
            Ok(database) => return Ok(database),
            Err(DatabaseError::DatabaseAlreadyOpen) if attempt < DATABASE_OPEN_RETRIES => {
                std::thread::sleep(DATABASE_OPEN_RETRY_DELAY);
            }
            Err(DatabaseError::DatabaseAlreadyOpen) => {
                return Err(StoreError::BackendUnavailable);
            }
            Err(error) => return Err(map_database_error(error)),
        }
    }
    Err(StoreError::BackendUnavailable)
}

fn wait_for_initialized_table(path: &Path) -> Result<bool, StoreError> {
    for attempt in 0..=DATABASE_OPEN_RETRIES {
        let database = open_existing_database(path)?;
        let initialized = database_has_table(&database)?;
        drop(database);
        if initialized {
            return Ok(true);
        }
        if attempt < DATABASE_OPEN_RETRIES {
            std::thread::sleep(DATABASE_OPEN_RETRY_DELAY);
        }
    }
    Ok(false)
}

fn initialize_table(database: &Database) -> Result<(), StoreError> {
    let write = database.begin_write().map_err(map_transaction_error)?;
    {
        let _table = write.open_table(RUNS).map_err(map_table_error)?;
    }
    write.commit().map_err(map_commit_error)
}

fn encode_state(state: &DurableRunState) -> Result<Vec<u8>, StoreError> {
    let payload = postcard::to_allocvec(state).map_err(|error| {
        StoreError::IoFailure(format!("durable state encoding failed: {error:?}"))
    })?;
    let mut encoded = Vec::with_capacity(FORMAT_MAGIC.len() + 2 + CHECKSUM_LEN + payload.len());
    encoded.extend_from_slice(FORMAT_MAGIC);
    encoded.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    encoded.extend_from_slice(&checksum(&payload).to_le_bytes());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

fn decode_state(encoded: &[u8]) -> Result<DurableRunState, StoreError> {
    let header_len = FORMAT_MAGIC.len() + std::mem::size_of::<u16>() + CHECKSUM_LEN;
    if encoded.len() < header_len || !encoded.starts_with(FORMAT_MAGIC) {
        return Err(StoreError::DataCorruption(
            "durable state schema header is invalid".to_string(),
        ));
    }
    let version_offset = FORMAT_MAGIC.len();
    let version = u16::from_le_bytes([encoded[version_offset], encoded[version_offset + 1]]);
    if version != FORMAT_VERSION {
        return Err(StoreError::DataCorruption(format!(
            "unsupported durable state schema version {version}"
        )));
    }
    let checksum_offset = FORMAT_MAGIC.len() + std::mem::size_of::<u16>();
    let expected_checksum = u64::from_le_bytes(
        encoded[checksum_offset..checksum_offset + CHECKSUM_LEN]
            .try_into()
            .expect("checksum length is fixed"),
    );
    if expected_checksum != checksum(&encoded[header_len..]) {
        return Err(StoreError::DataCorruption(
            "durable state checksum mismatch".to_string(),
        ));
    }
    let (state, remainder) = postcard::take_from_bytes::<DurableRunState>(&encoded[header_len..])
        .map_err(|error| {
        StoreError::DataCorruption(format!("durable state payload is invalid: {error:?}"))
    })?;
    if !remainder.is_empty() {
        return Err(StoreError::DataCorruption(
            "durable state payload has trailing bytes".to_string(),
        ));
    }
    state.validate_persisted()?;
    Ok(state)
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn map_io_error(error: io::Error) -> StoreError {
    let message = error.to_string();
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            StoreError::BackendUnavailable
        }
        io::ErrorKind::PermissionDenied => StoreError::IoFailure(message),
        io::ErrorKind::InvalidData => StoreError::DataCorruption(message),
        _ => StoreError::IoFailure(message),
    }
}

fn map_storage_error(error: redb::StorageError) -> StoreError {
    match error {
        redb::StorageError::Corrupted(message) => StoreError::DataCorruption(message),
        redb::StorageError::Io(error) => map_io_error(error),
        redb::StorageError::PreviousIo => {
            StoreError::IoFailure("redb reported a previous I/O failure".to_string())
        }
        redb::StorageError::ValueTooLarge(length) => {
            StoreError::IoFailure(format!("durable value is too large: {length} bytes"))
        }
        redb::StorageError::LockPoisoned(_) => StoreError::BackendUnavailable,
        error => StoreError::IoFailure(error.to_string()),
    }
}

fn map_database_error(error: DatabaseError) -> StoreError {
    match error {
        DatabaseError::DatabaseAlreadyOpen => StoreError::BackendUnavailable,
        DatabaseError::Storage(error) => map_storage_error(error),
        DatabaseError::UpgradeRequired(version) => StoreError::DataCorruption(format!(
            "redb database format {version} requires an unsupported upgrade"
        )),
        DatabaseError::RepairAborted => {
            StoreError::IoFailure("redb database repair was aborted".to_string())
        }
        error => StoreError::IoFailure(error.to_string()),
    }
}

fn map_transaction_error(error: redb::TransactionError) -> StoreError {
    match error {
        redb::TransactionError::Storage(error) => map_storage_error(error),
        redb::TransactionError::ReadTransactionStillInUse(_) => {
            StoreError::IoFailure("redb read transaction is still in use".to_string())
        }
        error => StoreError::IoFailure(error.to_string()),
    }
}

fn map_commit_error(error: redb::CommitError) -> StoreError {
    match error {
        redb::CommitError::Storage(error) => map_storage_error(error),
        error => StoreError::IoFailure(error.to_string()),
    }
}

fn map_table_error(error: redb::TableError) -> StoreError {
    match error {
        redb::TableError::Storage(error) => map_storage_error(error),
        error => StoreError::DataCorruption(error.to_string()),
    }
}

impl Display for FileDurableStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("FileDurableStore")
            .field(&self.path)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_categories_do_not_treat_permission_as_backend_contention() {
        assert_eq!(
            map_io_error(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission"
            ))
            .kind(),
            crate::StoreErrorKind::IoFailure
        );
        assert_eq!(
            map_io_error(io::Error::new(io::ErrorKind::WouldBlock, "locked")).kind(),
            crate::StoreErrorKind::BackendUnavailable
        );
    }

    #[test]
    fn encoded_state_checksum_rejects_payload_mutation() {
        let state = DurableRunState::new(crate::RunId::new("checksum-run").expect("run is valid"));
        let mut encoded = encode_state(&state).expect("state encodes");
        let last = encoded.last_mut().expect("payload is non-empty");
        *last ^= 1;

        assert_eq!(
            decode_state(&encoded)
                .expect_err("mutated payload is rejected")
                .kind(),
            crate::StoreErrorKind::DataCorruption
        );
    }
}
