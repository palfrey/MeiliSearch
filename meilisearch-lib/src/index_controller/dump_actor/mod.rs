use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use log::{info, trace, warn};
use serde::{Deserialize, Serialize};
use tokio::fs::create_dir_all;

use loaders::v1::MetadataV1;

pub use actor::DumpActor;
pub use handle_impl::*;
pub use message::DumpMsg;

use super::index_resolver::HardStateIndexResolver;
use super::updates::UpdateSender;
use crate::compression::{from_tar_gz, to_tar_gz};
use crate::index_controller::dump_actor::error::DumpActorError;
use crate::index_controller::dump_actor::loaders::{v2, v3};
use crate::index_controller::updates::UpdateMsg;
use crate::options::IndexerOpts;
use error::Result;

mod actor;
pub mod error;
mod handle_impl;
mod loaders;
mod message;

const META_FILE_NAME: &str = "metadata.json";

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    db_version: String,
    index_db_size: usize,
    update_db_size: usize,
    dump_date: DateTime<Utc>,
}

impl Metadata {
    pub fn new(index_db_size: usize, update_db_size: usize) -> Self {
        Self {
            db_version: env!("CARGO_PKG_VERSION").to_string(),
            index_db_size,
            update_db_size,
            dump_date: Utc::now(),
        }
    }
}

#[async_trait::async_trait]
pub trait DumpActorHandle {
    /// Start the creation of a dump
    /// Implementation: [handle_impl::DumpActorHandleImpl::create_dump]
    async fn create_dump(&self) -> Result<DumpInfo>;

    /// Return the status of an already created dump
    /// Implementation: [handle_impl::DumpActorHandleImpl::dump_info]
    async fn dump_info(&self, uid: String) -> Result<DumpInfo>;
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "dumpVersion")]
pub enum MetadataVersion {
    V1(MetadataV1),
    V2(Metadata),
    V3(Metadata),
}

impl MetadataVersion {
    pub fn new_v3(index_db_size: usize, update_db_size: usize) -> Self {
        let meta = Metadata::new(index_db_size, update_db_size);
        Self::V3(meta)
    }

    pub fn db_version(&self) -> &str {
        match self {
            Self::V1(meta) => &meta.db_version,
            Self::V2(meta) | Self::V3(meta) => &meta.db_version,
        }
    }

    pub fn version(&self) -> &str {
        match self {
            MetadataVersion::V1(_) => "V1",
            MetadataVersion::V2(_) => "V2",
            MetadataVersion::V3(_) => "V3",
        }
    }

    pub fn dump_date(&self) -> Option<&DateTime<Utc>> {
        match self {
            MetadataVersion::V1(_) => None,
            MetadataVersion::V2(meta) | MetadataVersion::V3(meta) => Some(&meta.dump_date),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub enum DumpStatus {
    Done,
    InProgress,
    Failed,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DumpInfo {
    pub uid: String,
    pub status: DumpStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<DateTime<Utc>>,
}

impl DumpInfo {
    pub fn new(uid: String, status: DumpStatus) -> Self {
        Self {
            uid,
            status,
            error: None,
            started_at: Utc::now(),
            finished_at: None,
        }
    }

    pub fn with_error(&mut self, error: String) {
        self.status = DumpStatus::Failed;
        self.finished_at = Some(Utc::now());
        self.error = Some(error);
    }

    pub fn done(&mut self) {
        self.finished_at = Some(Utc::now());
        self.status = DumpStatus::Done;
    }

    pub fn dump_already_in_progress(&self) -> bool {
        self.status == DumpStatus::InProgress
    }
}

pub fn load_dump(
    dst_path: impl AsRef<Path>,
    src_path: impl AsRef<Path>,
    index_db_size: usize,
    update_db_size: usize,
    indexer_opts: &IndexerOpts,
) -> anyhow::Result<()> {
    // Setup a temp directory path in the same path as the database, to prevent cross devices
    // references.
    let temp_path = dst_path
        .as_ref()
        .parent()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| ".".into());
    if cfg!(windows) {
        std::env::set_var("TMP", temp_path);
    } else {
        std::env::set_var("TMPDIR", temp_path);
    }

    let tmp_src = tempfile::tempdir()?;
    let tmp_src_path = tmp_src.path();

    from_tar_gz(&src_path, tmp_src_path)?;

    let meta_path = tmp_src_path.join(META_FILE_NAME);
    let mut meta_file = File::open(&meta_path)?;
    let meta: MetadataVersion = serde_json::from_reader(&mut meta_file)?;

    let tmp_dst = tempfile::tempdir()?;

    info!(
        "Loading dump {}, dump database version: {}, dump version: {}",
        meta.dump_date()
            .map(|t| format!("from {}", t))
            .unwrap_or_else(String::new),
        meta.db_version(),
        meta.version()
    );

    match meta {
        MetadataVersion::V1(meta) => {
            meta.load_dump(&tmp_src_path, tmp_dst.path(), index_db_size, indexer_opts)?
        }
        MetadataVersion::V2(meta) => v2::load_dump(
            meta,
            &tmp_src_path,
            tmp_dst.path(),
            index_db_size,
            update_db_size,
            indexer_opts,
        )?,
        MetadataVersion::V3(meta) => v3::load_dump(
            meta,
            &tmp_src_path,
            tmp_dst.path(),
            index_db_size,
            update_db_size,
            indexer_opts,
        )?,
    }
    // Persist and atomically rename the db
    let persisted_dump = tmp_dst.into_path();
    if dst_path.as_ref().exists() {
        warn!("Overwriting database at {}", dst_path.as_ref().display());
        std::fs::remove_dir_all(&dst_path)?;
    }

    std::fs::rename(&persisted_dump, &dst_path)?;

    Ok(())
}

struct DumpTask {
    path: PathBuf,
    index_resolver: Arc<HardStateIndexResolver>,
    update_handle: UpdateSender,
    uid: String,
    update_db_size: usize,
    index_db_size: usize,
}

impl DumpTask {
    async fn run(self) -> Result<()> {
        trace!("Performing dump.");

        create_dir_all(&self.path).await?;

        let temp_dump_dir = tokio::task::spawn_blocking(tempfile::TempDir::new).await??;
        let temp_dump_path = temp_dump_dir.path().to_owned();

        let meta = MetadataVersion::new_v3(self.index_db_size, self.update_db_size);
        let meta_path = temp_dump_path.join(META_FILE_NAME);
        let mut meta_file = File::create(&meta_path)?;
        serde_json::to_writer(&mut meta_file, &meta)?;

        let uuids = self.index_resolver.dump(temp_dump_path.clone()).await?;

        UpdateMsg::dump(&self.update_handle, uuids, temp_dump_path.clone()).await?;

        let dump_path = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let temp_dump_file = tempfile::NamedTempFile::new()?;
            to_tar_gz(temp_dump_path, temp_dump_file.path())
                .map_err(|e| DumpActorError::Internal(e.into()))?;

            let dump_path = self.path.join(self.uid).with_extension("dump");
            temp_dump_file.persist(&dump_path)?;

            Ok(dump_path)
        })
        .await??;

        info!("Created dump in {:?}.", dump_path);

        Ok(())
    }
}
