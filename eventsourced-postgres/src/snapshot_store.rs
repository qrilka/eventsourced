//! A [SnapshotStore] implementation based on [PostgreSQL](https://www.postgresql.org/).

use crate::{Cnn, CnnPool, Error};
use bb8_postgres::{bb8::Pool, PostgresConnectionManager};
use bytes::Bytes;
use eventsourced::{SeqNo, Snapshot, SnapshotStore};
use serde::{Deserialize, Serialize};
use std::{
    error::Error as StdError,
    fmt::{self, Debug, Formatter},
};
use tokio_postgres::NoTls;
use tracing::debug;
use uuid::Uuid;

/// A [SnapshotStore] implementation based on [PostgreSQL](https://www.postgresql.org/).
#[derive(Clone)]
pub struct PostgresSnapshotStore {
    cnn_pool: CnnPool<NoTls>,
}

impl PostgresSnapshotStore {
    #[allow(missing_docs)]
    pub async fn new(config: Config) -> Result<Self, Error> {
        debug!(?config, "creating PostgresSnapshotStore");

        // Create connection pool.
        let tls = NoTls;
        let cnn_manager = PostgresConnectionManager::new_from_stringlike(config.cnn_config(), tls)
            .map_err(|error| {
                Error::Postgres("cannot create connection manager".to_string(), error)
            })?;
        let cnn_pool = Pool::builder()
            .build(cnn_manager)
            .await
            .map_err(|error| Error::Postgres("cannot create connection pool".to_string(), error))?;

        // Setup tables.
        if config.setup {
            cnn_pool
                .get()
                .await
                .map_err(Error::GetConnection)?
                .execute(
                    &include_str!("create_snapshot_store.sql")
                        .replace("snapshots", &config.snapshots_table),
                    &[],
                )
                .await
                .map_err(|error| Error::Postgres("cannot execute query".to_string(), error))?;
        }

        Ok(Self { cnn_pool })
    }

    async fn cnn(&self) -> Result<Cnn<NoTls>, Error> {
        self.cnn_pool.get().await.map_err(Error::GetConnection)
    }
}

impl Debug for PostgresSnapshotStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresSnapshotStore").finish()
    }
}

impl SnapshotStore for PostgresSnapshotStore {
    type Error = Error;

    async fn save<S, ToBytes, ToBytesError>(
        &mut self,
        id: Uuid,
        seq_no: SeqNo,
        state: S,
        to_bytes: &ToBytes,
    ) -> Result<(), Self::Error>
    where
        S: Send,
        ToBytes: Fn(&S) -> Result<Bytes, ToBytesError> + Sync,
        ToBytesError: StdError + Send + Sync + 'static,
    {
        debug!(%id, %seq_no, "saving snapshot");

        let bytes = to_bytes(&state).map_err(|source| Error::ToBytes(Box::new(source)))?;
        self.cnn()
            .await?
            .execute(
                "INSERT INTO snapshots VALUES ($1, $2, $3)",
                &[&id, &(seq_no.as_u64() as i64), &bytes.as_ref()],
            )
            .await
            .map_err(|error| Error::Postgres("cannot execute query".to_string(), error))
            .map(|_| ())
    }

    async fn load<S, FromBytes, FromBytesError>(
        &self,
        id: Uuid,
        from_bytes: FromBytes,
    ) -> Result<Option<Snapshot<S>>, Self::Error>
    where
        FromBytes: Fn(Bytes) -> Result<S, FromBytesError> + Send,
        FromBytesError: StdError + Send + Sync + 'static,
    {
        debug!(%id, "loading snapshot");

        self.cnn()
            .await?
            .query_opt(
                "SELECT seq_no, state FROM snapshots
                 WHERE id = $1
                 AND seq_no = (select max(seq_no) from snapshots where id = $1)",
                &[&id],
            )
            .await
            .map_err(|error| Error::Postgres("cannot execute query".to_string(), error))?
            .map(move |row| {
                let seq_no = (row.get::<_, i64>(0) as u64)
                    .try_into()
                    .map_err(|_| Error::ZeroSeqNo)?;
                let bytes = row.get::<_, &[u8]>(1);
                let bytes = Bytes::copy_from_slice(bytes);
                from_bytes(bytes)
                    .map_err(|source| Error::FromBytes(Box::new(source)))
                    .map(|state| Snapshot::new(seq_no, state))
            })
            .transpose()
    }
}

/// Configuration for the [PostgresSnapshotStore].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    host: String,

    port: u16,

    user: String,

    password: String,

    dbname: String,

    sslmode: String,

    #[serde(default = "snapshots_table_default")]
    snapshots_table: String,

    #[serde(default)]
    setup: bool,
}

impl Config {
    /// Change the `host`.
    pub fn with_host<T>(self, host: T) -> Self
    where
        T: ToString,
    {
        let host = host.to_string();
        Self { host, ..self }
    }

    /// Change the `port`.
    pub fn with_port(self, port: u16) -> Self {
        Self { port, ..self }
    }

    /// Change the `user`.
    pub fn with_user<T>(self, user: T) -> Self
    where
        T: ToString,
    {
        let user = user.to_string();
        Self { user, ..self }
    }

    /// Change the `password`.
    pub fn with_password<T>(self, password: T) -> Self
    where
        T: ToString,
    {
        let password = password.to_string();
        Self { password, ..self }
    }

    /// Change the `dbname`.
    pub fn with_dbname<T>(self, dbname: T) -> Self
    where
        T: ToString,
    {
        let dbname = dbname.to_string();
        Self { dbname, ..self }
    }

    /// Change the `sslmode`.
    pub fn with_sslmode<T>(self, sslmode: T) -> Self
    where
        T: ToString,
    {
        let sslmode = sslmode.to_string();
        Self { sslmode, ..self }
    }

    /// Change the `snapshots_table`.
    pub fn with_snapshots_table(self, snapshots_table: String) -> Self {
        Self {
            snapshots_table,
            ..self
        }
    }

    /// Change the `setup` flag.
    pub fn with_setup(self, setup: bool) -> Self {
        Self { setup, ..self }
    }

    fn cnn_config(&self) -> String {
        format!(
            "host={} port={} user={} password={} dbname={} sslmode={}",
            self.host, self.port, self.user, self.password, self.dbname, self.sslmode
        )
    }
}

impl Default for Config {
    /// Default values suitable for local testing only.
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 5432,
            user: "postgres".to_string(),
            password: "".to_string(),
            dbname: "postgres".to_string(),
            sslmode: "prefer".to_string(),
            snapshots_table: snapshots_table_default(),
            setup: false,
        }
    }
}

fn snapshots_table_default() -> String {
    "snapshots".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eventsourced::convert;
    use testcontainers::clients::Cli;
    use testcontainers_modules::postgres::Postgres;

    #[tokio::test]
    async fn test_snapshot_store() -> Result<(), Box<dyn StdError + Send + Sync>> {
        let client = Cli::default();
        let container = client.run(Postgres::default());
        let port = container.get_host_port_ipv4(5432);

        let config = Config::default().with_port(port).with_setup(true);
        let mut snapshot_store = PostgresSnapshotStore::new(config).await?;

        let id = Uuid::now_v7();

        let snapshot = snapshot_store
            .load::<i32, _, _>(id, &convert::prost::from_bytes)
            .await?;
        assert!(snapshot.is_none());

        let seq_no = 42.try_into().unwrap();
        let state = 666;

        snapshot_store
            .save(id, seq_no, state, &convert::prost::to_bytes)
            .await?;

        let snapshot = snapshot_store
            .load::<i32, _, _>(id, &convert::prost::from_bytes)
            .await?;

        assert!(snapshot.is_some());
        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.seq_no, seq_no);
        assert_eq!(snapshot.state, state);

        Ok(())
    }
}
