//! Event sourced entities.

#![allow(incomplete_features)]
#![feature(async_fn_in_trait)]
#![feature(return_position_impl_trait_in_trait)]
#![allow(clippy::type_complexity)]

pub mod convert;
mod evt_log;
mod snapshot_store;

pub use evt_log::*;
pub use snapshot_store::*;

use bytes::Bytes;
use futures::StreamExt;
use std::{any::Any, fmt::Debug};
use thiserror::Error;
use tokio::{
    pin,
    sync::{mpsc, oneshot},
    task,
};
use tracing::{debug, error};
use uuid::Uuid;

/// Command and event handling for an event sourced [Entity].
pub trait EventSourced {
    /// Command type.
    type Cmd: Debug;

    /// Event type.
    type Evt: Debug;

    /// Snapshot state type.
    type State;

    /// Error type for the command handler.
    type Error: std::error::Error;

    /// Command handler, returning the to be persisted events.
    fn handle_cmd(&self, cmd: Self::Cmd) -> Result<Vec<Self::Evt>, Self::Error>;

    /// Event handler, returning whether to take a snapshot or not.
    fn handle_evt(&mut self, seq_no: u64, evt: &Self::Evt) -> Option<Self::State>;

    /// Snapshot state handler.
    fn set_state(&mut self, state: Self::State);
}

/// An event sourced entity, uniquely identified by its ID.
///
/// Commands are handled by the command handler of its [EventSourced] value. Valid commands may
/// produce events which get persisted along with an increasing sequence number to its [EvtLog] and
/// then applied to the event handler of its [EventSourced] value. The event handler may decide to
/// save a snapshot at the current sequence number which is used to speed up spawning.
pub struct Entity<E, L, S, EvtToBytes, StateToBytes> {
    id: Uuid,
    seq_no: u64,
    event_sourced: E,
    evt_log: L,
    snapshot_store: S,
    evt_to_bytes: EvtToBytes,
    state_to_bytes: StateToBytes,
}

impl<E, L, S, EvtToBytes, EvtToBytesError, StateToBytes, StateToBytesError>
    Entity<E, L, S, EvtToBytes, StateToBytes>
where
    E: EventSourced + Send + 'static,
    E::Cmd: Send + 'static,
    E::Evt: Send + Sync,
    E::State: Send + Sync,
    E::Error: Send,
    L: EvtLog + Send + 'static,
    S: SnapshotStore + Send + 'static,
    EvtToBytes: Fn(&E::Evt) -> Result<Bytes, EvtToBytesError> + Send + Sync + 'static,
    EvtToBytesError: std::error::Error + Send + Sync + 'static,
    StateToBytes: Fn(&E::State) -> Result<Bytes, StateToBytesError> + Send + Sync + 'static,
    StateToBytesError: std::error::Error + Send + Sync + 'static,
{
    /// Spawns an event sourced [Entity] with the given ID and creates an [EntityRef] for it.
    ///
    /// Commands can be sent by invoking `handle_cmd` on the returned [EntityRef] which uses a
    /// buffered channel with the given size.
    ///
    /// First the given [SnapshotStore] is used to find and possibly load a snapshot. Then the
    /// [EvtLog] is used to find the last sequence number and then to load any remaining events.
    pub async fn spawn<EvtFromBytes, EvtFromBytesError, StateFromBytes, StateFromBytesError>(
        id: Uuid,
        mut event_sourced: E,
        buffer: usize,
        evt_log: L,
        snapshot_store: S,
        binarizer: Binarizer<EvtToBytes, EvtFromBytes, StateToBytes, StateFromBytes>,
    ) -> Result<EntityRef<E>, SpawnEntityError<L, S>>
    where
        EvtFromBytes: Fn(Bytes) -> Result<E::Evt, EvtFromBytesError> + Copy + Send + Sync + 'static,
        EvtFromBytesError: std::error::Error + Send + Sync + 'static,
        StateFromBytes: Fn(Bytes) -> Result<E::State, StateFromBytesError> + Send + Sync + 'static,
        StateFromBytesError: std::error::Error + Send + Sync + 'static,
    {
        assert!(buffer >= 1, "buffer must be positive");

        let Binarizer {
            evt_to_bytes,
            evt_from_bytes,
            state_to_bytes,
            state_from_bytes,
        } = binarizer;

        // Restore snapshot.
        let (snapshot_seq_no, metadata) = snapshot_store
            .load::<E::State, _, _>(id, &state_from_bytes)
            .await
            .map_err(SpawnEntityError::LoadSnapshot)?
            .map(
                |Snapshot {
                     seq_no,
                     state,
                     metadata,
                 }| {
                    debug!(%id, seq_no, "Restoring snapshot");
                    event_sourced.set_state(state);
                    (seq_no, metadata)
                },
            )
            .unwrap_or((0, None));

        // Replay latest events.
        let last_seq_no = evt_log
            .last_seq_no(id)
            .await
            .map_err(SpawnEntityError::LastSeqNo)?;
        assert!(
            snapshot_seq_no <= last_seq_no,
            "snapshot_seq_no must be less than or equal to last_seq_no"
        );
        if snapshot_seq_no < last_seq_no {
            let from_seq_no = snapshot_seq_no + 1;
            debug!(%id, from_seq_no, last_seq_no , "Replaying evts");
            let evts = evt_log
                .evts_by_id::<E::Evt, _, _>(id, from_seq_no, last_seq_no, metadata, evt_from_bytes)
                .await
                .map_err(SpawnEntityError::EvtsById)?;
            pin!(evts);
            while let Some(evt) = evts.next().await {
                let (seq_no, evt) = evt.map_err(SpawnEntityError::NextEvt)?;
                event_sourced.handle_evt(seq_no, &evt);
            }
        }

        // Create entity.
        let mut entity = Entity {
            id,
            seq_no: last_seq_no,
            event_sourced,
            evt_log,
            snapshot_store,
            evt_to_bytes,
            state_to_bytes,
        };
        debug!(%id, "Entity created");

        let (cmd_in, mut cmd_out) =
            mpsc::channel::<(E::Cmd, oneshot::Sender<Result<Vec<E::Evt>, E::Error>>)>(buffer);

        // Spawn handler loop.
        task::spawn(async move {
            while let Some((cmd, result_sender)) = cmd_out.recv().await {
                match entity.handle_cmd(cmd).await {
                    Ok((next_entity, result)) => {
                        entity = next_entity;
                        if result_sender.send(result).is_err() {
                            error!(%id, "Cannot send command handler result");
                        };
                    }
                    Err(error) => {
                        error!(%id, %error, "Cannot persist events");
                        break;
                    }
                }
            }
            debug!(%id, "Entity terminated");
        });

        Ok(EntityRef { id, cmd_in })
    }

    async fn handle_cmd(
        mut self,
        cmd: E::Cmd,
    ) -> Result<(Self, Result<Vec<E::Evt>, E::Error>), Box<dyn std::error::Error>> {
        // TODO Remove this helper once async fn in trait is stable!
        fn make_send<T, E>(
            f: impl std::future::Future<Output = Result<T, E>> + Send,
        ) -> impl std::future::Future<Output = Result<T, E>> + Send {
            f
        }

        // Handle command
        let evts = match self.event_sourced.handle_cmd(cmd) {
            Ok(evts) => evts,
            Err(error) => return Ok((self, Err(error))),
        };

        if !evts.is_empty() {
            // Persist events
            // TODO Remove this helper once async fn in trait is stable!
            let send_fut =
                make_send(
                    self.evt_log
                        .persist(self.id, &evts, self.seq_no, &self.evt_to_bytes),
                );
            let metadata = send_fut.await?;

            // Handle persisted events
            let state = evts.iter().fold(None, |state, evt| {
                self.seq_no += 1;
                self.event_sourced.handle_evt(self.seq_no, evt).or(state)
            });

            // Persist latest snapshot if any
            if let Some(state) = state {
                debug!(id = %self.id, seq_no = self.seq_no, "Saving snapshot");
                // TODO Remove this helper once async fn in trait is stable!
                let send_fut = make_send(self.snapshot_store.save(
                    self.id,
                    self.seq_no,
                    &state,
                    metadata,
                    &self.state_to_bytes,
                ));
                send_fut.await?;
            }
        }

        Ok((self, Ok(evts)))
    }
}

/// Errors from spawning an event sourced [Entity].
#[derive(Debug, Error)]
pub enum SpawnEntityError<L, S>
where
    L: EvtLog + 'static,
    S: SnapshotStore + 'static,
{
    /// A snapshot cannot be loaded from the snapshot store.
    #[error("Cannot load snapshot from snapshot store")]
    LoadSnapshot(#[source] S::Error),

    /// The last seqence number cannot be obtained from the event log.
    #[error("Cannot get last seqence number from event log")]
    LastSeqNo(#[source] L::Error),

    /// Events by ID cannot be obtained from the event log.
    #[error("Cannot get events by ID from event log")]
    EvtsById(#[source] L::Error),

    /// The next event cannot be obtained from the event log.
    #[error("Cannot get next event from event log")]
    NextEvt(#[source] L::Error),
}

/// A proxy to a spawned event sourced [Entity] which can be used to invoke its command handler.
#[derive(Debug, Clone)]
pub struct EntityRef<E>
where
    E: EventSourced,
{
    id: Uuid,
    cmd_in: mpsc::Sender<(E::Cmd, oneshot::Sender<Result<Vec<E::Evt>, E::Error>>)>,
}

impl<E> EntityRef<E>
where
    E: EventSourced + 'static,
{
    /// Get the ID of the proxied event sourced [Entity].
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Invoke the command handler of the proxied event sourced [Entity].
    pub async fn handle_cmd(&self, cmd: E::Cmd) -> Result<Vec<E::Evt>, EntityRefError<E>> {
        let (result_in, result_out) = oneshot::channel();
        self.cmd_in.send((cmd, result_in)).await?;
        result_out.await?.map_err(EntityRefError::InvalidCommand)
    }
}

/// Errors from an [EntityRef].
#[derive(Debug, Error)]
pub enum EntityRefError<E>
where
    E: EventSourced + 'static,
{
    /// An invalid command has been rejected by a command hander. This is considered a client
    /// error, like 400 Bad Request, i.e. normal behavior of the event sourced [Entity] and its
    /// [EntityRef].
    #[error("Invalid command rejected by command handler")]
    InvalidCommand(#[source] E::Error),

    /// A command cannot be sent from an [EntityRef] to its [Entity]. This is considered an
    /// internal error, like 500 Internal Server Error, i.e. erroneous behavior of the event
    /// sourced [Entity] and its [EntityRef].
    #[error("Cannot send command to Entity")]
    SendCmd(
        #[from] mpsc::error::SendError<(E::Cmd, oneshot::Sender<Result<Vec<E::Evt>, E::Error>>)>,
    ),

    /// An [EntityRef] cannot receive the command handler result from its [Entity], potentially
    /// because its entity has terminated. This is considered an internal error, like 500 Internal
    /// Server Error, i.e. erroneous behavior of the event sourced [Entity] and its [EntityRef].
    #[error("Cannot receive command handler result from Entity")]
    EntityTerminated(#[from] oneshot::error::RecvError),
}

/// Optional metadata to optimize sequence number based lookup of events in the [EvtLog].
pub type Metadata = Option<Box<dyn Any + Send>>;

/// Collection of conversion functions from and to [Bytes](bytes::Bytes) for events and snapshots.
pub struct Binarizer<EvtToBytes, EvtFromBytes, StateToBytes, StateFromBytes> {
    pub evt_to_bytes: EvtToBytes,
    pub evt_from_bytes: EvtFromBytes,
    pub state_to_bytes: StateToBytes,
    pub state_from_bytes: StateFromBytes,
}

#[cfg(all(test, feature = "prost"))]
mod tests {
    use super::*;
    use futures::{stream, Stream};

    mod counter {
        #![allow(unused)]
        use crate::EventSourced;
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../include/counter.rs"
        ));
    }
    use bytes::BytesMut;
    use counter::*;
    use prost::Message;
    use std::future::ready;

    #[derive(Debug)]
    struct TestEvtLog;

    impl EvtLog for TestEvtLog {
        type Error = TestEvtLogError;

        async fn persist<'a, 'b, 'c, E, ToBytes, ToBytesError>(
            &'a self,
            _id: Uuid,
            _evts: &'b [E],
            _last_seq_no: u64,
            _to_bytes: &'c ToBytes,
        ) -> Result<Metadata, Self::Error>
        where
            'b: 'a,
            'c: 'a,
            E: Send + Sync + 'a,
            ToBytes: Fn(&E) -> Result<Bytes, ToBytesError> + Send + Sync,
            ToBytesError: std::error::Error + Send + Sync + 'static,
        {
            Ok(None)
        }

        async fn last_seq_no(&self, _entity_id: Uuid) -> Result<u64, Self::Error> {
            Ok(43)
        }

        async fn evts_by_id<'a, E, EvtFromBytes, EvtFromBytesError>(
            &'a self,
            _id: Uuid,
            from_seq_no: u64,
            to_seq_no: u64,
            _metadata: Metadata,
            evt_from_bytes: EvtFromBytes,
        ) -> Result<impl Stream<Item = Result<(u64, E), Self::Error>> + 'a, Self::Error>
        where
            E: Send + 'a,
            EvtFromBytes: Fn(Bytes) -> Result<E, EvtFromBytesError> + Send + Sync + 'static,
            EvtFromBytesError: std::error::Error + Send + Sync + 'static,
        {
            Ok(stream::iter(0..666)
                .skip_while(move |n| ready(*n < from_seq_no))
                .take_while(move |n| ready(*n <= to_seq_no))
                .map(move |n| {
                    let evt = Evt {
                        evt: Some(evt::Evt::Increased(Increased {
                            old_value: n,
                            inc: 1,
                        })),
                    };
                    let mut bytes = BytesMut::new();
                    evt.encode(&mut bytes).unwrap();
                    let evt = evt_from_bytes(bytes.into()).unwrap();
                    Ok((n + 1, evt))
                }))
        }
    }

    #[derive(Debug, Error)]
    #[error("TestEvtLogError")]
    struct TestEvtLogError;

    #[derive(Debug)]
    struct TestSnapshotStore;

    impl SnapshotStore for TestSnapshotStore {
        type Error = TestSnapshotStoreError;

        async fn save<'a, 'b, 'c, S, StateToBytes, StateToBytesError>(
            &'a mut self,
            _id: Uuid,
            _seq_no: u64,
            _state: &'b S,
            _metadata: Metadata,
            _state_to_bytes: &'c StateToBytes,
        ) -> Result<(), Self::Error>
        where
            'b: 'a,
            'c: 'a,
            S: Send + Sync + 'a,
            StateToBytes: Fn(&S) -> Result<Bytes, StateToBytesError> + Send + Sync + 'static,
            StateToBytesError: std::error::Error + Send + Sync + 'static,
        {
            Ok(())
        }

        async fn load<S, StateFromBytes, StateFromBytesError>(
            &self,
            _id: Uuid,
            state_from_bytes: &StateFromBytes,
        ) -> Result<Option<Snapshot<S>>, Self::Error>
        where
            StateFromBytes: Fn(Bytes) -> Result<S, StateFromBytesError> + Send + Sync + 'static,
            StateFromBytesError: std::error::Error + Send + Sync + 'static,
        {
            let mut bytes = BytesMut::new();
            42.encode(&mut bytes).unwrap();
            let state = state_from_bytes(bytes.into()).unwrap();
            Ok(Some(Snapshot {
                seq_no: 42,
                state,
                metadata: None,
            }))
        }
    }

    #[derive(Debug, Error)]
    #[error("TestSnapshotStoreError")]
    struct TestSnapshotStoreError;

    #[tokio::test]
    async fn test() -> Result<(), Box<dyn std::error::Error>> {
        let event_log = TestEvtLog;
        let snapshot_store = TestSnapshotStore;

        let entity = Entity::spawn(
            Uuid::now_v7(),
            Counter::default(),
            666,
            event_log,
            snapshot_store,
            convert::prost::binarizer(),
        )
        .await?;

        let evts = entity.handle_cmd(Cmd::Inc(1)).await?;
        assert_eq!(
            evts,
            vec![Evt {
                evt: Some(evt::Evt::Increased(Increased {
                    old_value: 43,
                    inc: 1,
                }))
            }]
        );

        let evts = entity.handle_cmd(Cmd::Dec(666)).await;
        assert!(evts.is_err());

        Ok(())
    }
}
