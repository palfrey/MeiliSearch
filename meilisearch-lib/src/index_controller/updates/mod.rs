pub mod error;
mod message;
pub mod status;
pub mod store;

use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use actix_web::error::PayloadError;
use async_stream::stream;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use log::trace;
use milli::update::IndexDocumentsMethod;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use self::error::{Result, UpdateLoopError};
pub use self::message::UpdateMsg;
use self::store::{UpdateStore, UpdateStoreInfo};
use crate::document_formats::{read_csv, read_json, read_ndjson};
use crate::index::{Index, Settings, Unchecked};
use crate::index_controller::update_file_store::UpdateFileStore;
use status::UpdateStatus;

use super::index_resolver::HardStateIndexResolver;
use super::{DocumentAdditionFormat, Update};

pub type UpdateSender = mpsc::Sender<UpdateMsg>;

pub fn create_update_handler(
    index_resolver: Arc<HardStateIndexResolver>,
    db_path: impl AsRef<Path>,
    update_store_size: usize,
) -> anyhow::Result<UpdateSender> {
    let path = db_path.as_ref().to_owned();
    let (sender, receiver) = mpsc::channel(100);
    let actor = UpdateLoop::new(update_store_size, receiver, path, index_resolver)?;

    tokio::task::spawn(actor.run());

    Ok(sender)
}

/// A wrapper type to implement read on a `Stream<Result<Bytes, Error>>`.
struct StreamReader<S> {
    stream: S,
    current: Option<Bytes>,
}

impl<S> StreamReader<S> {
    fn new(stream: S) -> Self {
        Self {
            stream,
            current: None,
        }
    }
}

impl<S: Stream<Item = std::result::Result<Bytes, PayloadError>> + Unpin> io::Read
    for StreamReader<S>
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // TODO: optimize buf filling
        match self.current.take() {
            Some(mut bytes) => {
                let split_at = bytes.len().min(buf.len());
                let copied = bytes.split_to(split_at);
                buf[..split_at].copy_from_slice(&copied);
                if !bytes.is_empty() {
                    self.current.replace(bytes);
                }
                Ok(copied.len())
            }
            None => match tokio::runtime::Handle::current().block_on(self.stream.next()) {
                Some(Ok(bytes)) => {
                    self.current.replace(bytes);
                    self.read(buf)
                }
                Some(Err(e)) => Err(io::Error::new(io::ErrorKind::BrokenPipe, e)),
                None => Ok(0),
            },
        }
    }
}

pub struct UpdateLoop {
    store: Arc<UpdateStore>,
    inbox: Option<mpsc::Receiver<UpdateMsg>>,
    update_file_store: UpdateFileStore,
    must_exit: Arc<AtomicBool>,
}

impl UpdateLoop {
    pub fn new(
        update_db_size: usize,
        inbox: mpsc::Receiver<UpdateMsg>,
        path: impl AsRef<Path>,
        index_resolver: Arc<HardStateIndexResolver>,
    ) -> anyhow::Result<Self> {
        let path = path.as_ref().to_owned();
        std::fs::create_dir_all(&path)?;

        let mut options = heed::EnvOpenOptions::new();
        options.map_size(update_db_size);

        let must_exit = Arc::new(AtomicBool::new(false));

        let update_file_store = UpdateFileStore::new(&path).unwrap();
        let store = UpdateStore::open(
            options,
            &path,
            index_resolver,
            must_exit.clone(),
            update_file_store.clone(),
        )?;

        let inbox = Some(inbox);

        Ok(Self {
            store,
            inbox,
            must_exit,
            update_file_store,
        })
    }

    pub async fn run(mut self) {
        use UpdateMsg::*;

        trace!("Started update actor.");

        let mut inbox = self
            .inbox
            .take()
            .expect("A receiver should be present by now.");

        let must_exit = self.must_exit.clone();
        let stream = stream! {
            loop {
                let msg = inbox.recv().await;

                if must_exit.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                match msg {
                    Some(msg) => yield msg,
                    None => break,
                }
            }
        };

        stream
            .for_each_concurrent(Some(10), |msg| async {
                match msg {
                    Update { uuid, update, ret } => {
                        let _ = ret.send(self.handle_update(uuid, update).await);
                    }
                    ListUpdates { uuid, ret } => {
                        let _ = ret.send(self.handle_list_updates(uuid).await);
                    }
                    GetUpdate { uuid, ret, id } => {
                        let _ = ret.send(self.handle_get_update(uuid, id).await);
                    }
                    DeleteIndex { uuid, ret } => {
                        let _ = ret.send(self.handle_delete(uuid).await);
                    }
                    Snapshot { indexes, path, ret } => {
                        let _ = ret.send(self.handle_snapshot(indexes, path).await);
                    }
                    GetInfo { ret } => {
                        let _ = ret.send(self.handle_get_info().await);
                    }
                    Dump { indexes, path, ret } => {
                        let _ = ret.send(self.handle_dump(indexes, path).await);
                    }
                }
            })
            .await;
    }

    async fn handle_update(&self, index_uuid: Uuid, update: Update) -> Result<UpdateStatus> {
        let registration = match update {
            Update::DocumentAddition {
                payload,
                primary_key,
                method,
                format,
            } => {
                let mut reader = BufReader::new(StreamReader::new(payload));
                let (content_uuid, mut update_file) = self.update_file_store.new_update()?;
                tokio::task::spawn_blocking(move || -> Result<_> {
                    // check if the payload is empty, and return an error
                    reader.fill_buf()?;
                    if reader.buffer().is_empty() {
                        return Err(UpdateLoopError::MissingPayload(format));
                    }

                    match format {
                        DocumentAdditionFormat::Json => read_json(reader, &mut *update_file)?,
                        DocumentAdditionFormat::Csv => read_csv(reader, &mut *update_file)?,
                        DocumentAdditionFormat::Ndjson => read_ndjson(reader, &mut *update_file)?,
                    }

                    update_file.persist()?;

                    Ok(())
                })
                .await??;

                store::Update::DocumentAddition {
                    primary_key,
                    method,
                    content_uuid,
                }
            }
            Update::Settings(settings) => store::Update::Settings(settings),
            Update::ClearDocuments => store::Update::ClearDocuments,
            Update::DeleteDocuments(ids) => store::Update::DeleteDocuments(ids),
        };

        let store = self.store.clone();
        let status =
            tokio::task::spawn_blocking(move || store.register_update(index_uuid, registration))
                .await??;

        Ok(status.into())
    }

    async fn handle_list_updates(&self, uuid: Uuid) -> Result<Vec<UpdateStatus>> {
        let update_store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let result = update_store.list(uuid)?;
            Ok(result)
        })
        .await?
    }

    async fn handle_get_update(&self, uuid: Uuid, id: u64) -> Result<UpdateStatus> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let result = store
                .meta(uuid, id)?
                .ok_or(UpdateLoopError::UnexistingUpdate(id))?;
            Ok(result)
        })
        .await?
    }

    async fn handle_delete(&self, uuid: Uuid) -> Result<()> {
        let store = self.store.clone();

        tokio::task::spawn_blocking(move || store.delete_all(uuid)).await??;

        Ok(())
    }

    async fn handle_snapshot(&self, indexes: Vec<Index>, path: PathBuf) -> Result<()> {
        let update_store = self.store.clone();

        tokio::task::spawn_blocking(move || update_store.snapshot(indexes, path)).await??;

        Ok(())
    }

    async fn handle_dump(&self, indexes: Vec<Index>, path: PathBuf) -> Result<()> {
        let update_store = self.store.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            update_store.dump(&indexes, path.to_path_buf())?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    async fn handle_get_info(&self) -> Result<UpdateStoreInfo> {
        let update_store = self.store.clone();
        let info = tokio::task::spawn_blocking(move || -> Result<UpdateStoreInfo> {
            let info = update_store.get_info()?;
            Ok(info)
        })
        .await??;

        Ok(info)
    }
}
