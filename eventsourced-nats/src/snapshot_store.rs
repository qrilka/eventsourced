//! A [SnapshotStore] implementation based on [NATS](https://nats.io/).

use crate::Error;
use async_nats::{
    connect,
    jetstream::{self, kv::Store, Context as Jetstream},
};
use bytes::{Bytes, BytesMut};
use eventsourced::{SeqNo, Snapshot, SnapshotStore};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{
    error::Error as StdError,
    fmt::{self, Debug, Formatter},
};
use tracing::debug;
use uuid::Uuid;

/// A [SnapshotStore] implementation based on [NATS](https://nats.io/).
#[derive(Clone)]
pub struct NatsSnapshotStore {
    jetstream: Jetstream,
    bucket: String,
}

impl NatsSnapshotStore {
    #[allow(missing_docs)]
    pub async fn new(config: Config) -> Result<Self, Error> {
        debug!(?config, "creating NatsSnapshotStore");

        let server_addr = config.server_addr;
        let client = connect(&server_addr).await.map_err(|error| {
            Error::Nats(
                format!("cannot connect to NATS server at {server_addr})"),
                error.into(),
            )
        })?;
        let jetstream = jetstream::new(client);

        // Setup bucket.
        if config.setup {
            let _ = jetstream
                .create_key_value(jetstream::kv::Config {
                    bucket: "snapshots".to_string(),
                    ..Default::default()
                })
                .await
                .map_err(|error| {
                    Error::Nats("cannot create NATS KV bucket".into(), error.into())
                })?;
        }

        Ok(Self {
            jetstream,
            bucket: config.bucket,
        })
    }

    async fn get_bucket(&self, name: &str) -> Result<Store, Error> {
        self.jetstream
            .get_key_value(name)
            .await
            .map_err(|error| Error::Nats("cannot get NATS KV bucket".into(), error.into()))
    }
}

impl Debug for NatsSnapshotStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsSnapshotStore")
            .field("bucket", &self.bucket)
            .finish()
    }
}

impl SnapshotStore for NatsSnapshotStore {
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
        let mut bytes = BytesMut::new();
        let state = to_bytes(&state).map_err(|error| Error::IntoBytes(Box::new(error)))?;
        let snapshot = proto::Snapshot {
            seq_no: seq_no.as_u64(),
            state,
        };
        snapshot.encode(&mut bytes)?;

        self.get_bucket(&self.bucket)
            .await?
            .put(id.to_string(), bytes.into())
            .await
            .map_err(|error| {
                Error::Nats(
                    "cannot store snapshot in NATS KV bucket".into(),
                    error.into(),
                )
            })?;
        debug!(%id, %seq_no, "saved snapshot");

        Ok(())
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
        let snapshot = self
            .get_bucket(&self.bucket)
            .await?
            .get(id.to_string())
            .await
            .map_err(|error| {
                Error::Nats(
                    "cannot load snapshot from NATS KV bucket".into(),
                    error.into(),
                )
            })?
            .map(|bytes| {
                proto::Snapshot::decode(bytes)
                    .map_err(Error::DecodeSnapshot)
                    .and_then(|proto::Snapshot { seq_no, state }| {
                        from_bytes(state)
                            .map_err(|error| Error::FromBytes(Box::new(error)))
                            .and_then(|state| {
                                seq_no
                                    .try_into()
                                    .map_err(Error::InvalidSeqNo)
                                    .map(|seq_no| Snapshot::new(seq_no, state))
                            })
                    })
            })
            .transpose()?;

        if snapshot.is_some() {
            debug!(%id, "loaded snapshot");
        } else {
            debug!(%id, "no snapshot to load");
        }

        Ok(snapshot)
    }
}

/// Configuration for the [SnapshotStore].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    server_addr: String,

    #[serde(default = "bucket_default")]
    bucket: String,

    #[serde(default)]
    setup: bool,
}

impl Config {
    /// Change the `server_addr`.
    pub fn with_server_addr<T>(self, server_addr: T) -> Self
    where
        T: ToString,
    {
        let server_addr = server_addr.to_string();
        Self {
            server_addr,
            ..self
        }
    }

    /// Change the `bucket`.
    pub fn with_bucket<T>(self, bucket: T) -> Self
    where
        T: ToString,
    {
        let bucket = bucket.to_string();
        Self { bucket, ..self }
    }

    /// Change the `setup` flag.
    pub fn with_setup(self, setup: bool) -> Self {
        Self { setup, ..self }
    }
}

impl Default for Config {
    /// Use "localhost:4222" for `server_addr` and "snapshots" for `bucket`.
    fn default() -> Self {
        Self {
            server_addr: "localhost:4222".to_string(),
            bucket: bucket_default(),
            setup: false,
        }
    }
}

fn bucket_default() -> String {
    "snapshots".to_string()
}

mod proto {
    include!(concat!(env!("OUT_DIR"), "/snapshot_store.rs"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::NATS_VERSION;
    use eventsourced::convert;
    use testcontainers::{clients::Cli, core::WaitFor};
    use testcontainers_modules::testcontainers::GenericImage;

    #[tokio::test]
    async fn test_snapshot_store() -> Result<(), Box<dyn StdError + Send + Sync>> {
        let client = Cli::default();
        let nats_image = GenericImage::new("nats", NATS_VERSION)
            .with_wait_for(WaitFor::message_on_stderr("Server is ready"));
        let container = client.run((nats_image, vec!["-js".to_string()]));
        let server_addr = format!("localhost:{}", container.get_host_port_ipv4(4222));

        let config = Config::default()
            .with_server_addr(server_addr)
            .with_setup(true);
        let mut snapshot_store = NatsSnapshotStore::new(config).await?;

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
