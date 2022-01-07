//! Repository implementation backed by memory

use futures_io::AsyncRead;
use futures_util::future::{BoxFuture, FutureExt};
use futures_util::io::{AsyncReadExt, Cursor};
use std::collections::HashMap;
use std::marker::PhantomData;

use crate::error::Error;
use crate::interchange::DataInterchange;
use crate::metadata::{MetadataPath, MetadataVersion, TargetPath};
use crate::repository::{RepositoryProvider, RepositoryStorage};
use crate::Result;

/// An ephemeral repository contained solely in memory.
#[derive(Debug)]
pub struct EphemeralRepository<D> {
    metadata: HashMap<(MetadataPath, MetadataVersion), Box<[u8]>>,
    targets: HashMap<TargetPath, Box<[u8]>>,
    _interchange: PhantomData<D>,
}

impl<D> EphemeralRepository<D>
where
    D: DataInterchange,
{
    /// Create a new ephemeral repository.
    pub fn new() -> Self {
        Self {
            metadata: HashMap::new(),
            targets: HashMap::new(),
            _interchange: PhantomData,
        }
    }

    /// Returns a [EphemeralTransaction] for manipulating this repository. This allows callers to
    /// stage a number of mutations, and optionally commit them all at once.
    pub fn transaction(&mut self) -> EphemeralTransaction<'_, D> {
        EphemeralTransaction {
            commit_repo: self,
            staging_repo: EphemeralRepository::new(),
        }
    }
}

impl<D> Default for EphemeralRepository<D>
where
    D: DataInterchange,
{
    fn default() -> Self {
        EphemeralRepository::new()
    }
}

impl<D> RepositoryProvider<D> for EphemeralRepository<D>
where
    D: DataInterchange + Sync,
{
    fn fetch_metadata<'a>(
        &'a self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
    ) -> BoxFuture<'a, Result<Box<dyn AsyncRead + Send + Unpin + 'a>>> {
        let bytes = match self.metadata.get(&(meta_path.clone(), version.clone())) {
            Some(bytes) => Ok(bytes),
            None => Err(Error::NotFound),
        };
        bytes_to_reader(bytes).boxed()
    }

    fn fetch_target<'a>(
        &'a self,
        target_path: &TargetPath,
    ) -> BoxFuture<'a, Result<Box<dyn AsyncRead + Send + Unpin + 'a>>> {
        let bytes = match self.targets.get(target_path) {
            Some(bytes) => Ok(bytes),
            None => Err(Error::NotFound),
        };
        bytes_to_reader(bytes).boxed()
    }
}

impl<D> RepositoryStorage<D> for EphemeralRepository<D>
where
    D: DataInterchange + Sync,
{
    fn store_metadata<'a>(
        &'a mut self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        metadata: &'a mut (dyn AsyncRead + Send + Unpin + 'a),
    ) -> BoxFuture<'a, Result<()>> {
        let meta_path = meta_path.clone();
        let version = version.clone();
        let self_metadata = &mut self.metadata;
        async move {
            let mut buf = Vec::new();
            metadata.read_to_end(&mut buf).await?;
            buf.shrink_to_fit();
            self_metadata.insert((meta_path, version), buf.into_boxed_slice());
            Ok(())
        }
        .boxed()
    }

    fn store_target<'a>(
        &'a mut self,
        target_path: &TargetPath,
        read: &'a mut (dyn AsyncRead + Send + Unpin + 'a),
    ) -> BoxFuture<'a, Result<()>> {
        let target_path = target_path.clone();
        let self_targets = &mut self.targets;
        async move {
            let mut buf = Vec::new();
            read.read_to_end(&mut buf).await?;
            buf.shrink_to_fit();
            self_targets.insert(target_path.clone(), buf.into_boxed_slice());
            Ok(())
        }
        .boxed()
    }
}

/// [EphemeralTransaction] is a special repository that is designed to atomically commit metadata
/// and targets to an [EphemeralRepository]. It can be used as a normal repository.
///
/// Note: `EphemeralTransaction::commit()` must be called in order to write the metadata and targets
/// to the [EphemeralRepository]. Otherwise any stored file will be lost on drop.
#[derive(Debug)]
pub struct EphemeralTransaction<'a, D> {
    commit_repo: &'a mut EphemeralRepository<D>,
    staging_repo: EphemeralRepository<D>,
}

impl<'a, D> EphemeralTransaction<'a, D>
where
    D: DataInterchange + Sync,
{
    /// Write all the metadata and targets in the transaction.
    pub fn commit(self) {
        self.commit_repo
            .metadata
            .extend(self.staging_repo.metadata.into_iter());

        self.commit_repo
            .targets
            .extend(self.staging_repo.targets.into_iter());
    }
}

impl<D> RepositoryProvider<D> for EphemeralTransaction<'_, D>
where
    D: DataInterchange + Sync,
{
    fn fetch_metadata<'a>(
        &'a self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
    ) -> BoxFuture<'a, Result<Box<dyn AsyncRead + Send + Unpin + 'a>>> {
        let key = (meta_path.clone(), version.clone());
        let bytes = if let Some(bytes) = self.staging_repo.metadata.get(&key) {
            Ok(bytes)
        } else {
            self.commit_repo.metadata.get(&key).ok_or(Error::NotFound)
        };
        bytes_to_reader(bytes).boxed()
    }

    fn fetch_target<'a>(
        &'a self,
        target_path: &TargetPath,
    ) -> BoxFuture<'a, Result<Box<dyn AsyncRead + Send + Unpin + 'a>>> {
        let bytes = if let Some(bytes) = self.staging_repo.targets.get(target_path) {
            Ok(bytes)
        } else {
            self.commit_repo
                .targets
                .get(target_path)
                .ok_or(Error::NotFound)
        };
        bytes_to_reader(bytes).boxed()
    }
}

impl<D> RepositoryStorage<D> for EphemeralTransaction<'_, D>
where
    D: DataInterchange + Sync,
{
    fn store_metadata<'a>(
        &'a mut self,
        meta_path: &MetadataPath,
        version: &MetadataVersion,
        metadata: &'a mut (dyn AsyncRead + Send + Unpin + 'a),
    ) -> BoxFuture<'a, Result<()>> {
        self.staging_repo
            .store_metadata(meta_path, version, metadata)
    }

    fn store_target<'a>(
        &'a mut self,
        target_path: &TargetPath,
        read: &'a mut (dyn AsyncRead + Send + Unpin + 'a),
    ) -> BoxFuture<'a, Result<()>> {
        self.staging_repo.store_target(target_path, read)
    }
}

#[allow(clippy::borrowed_box)]
async fn bytes_to_reader(
    bytes: Result<&'_ Box<[u8]>>,
) -> Result<Box<dyn AsyncRead + Send + Unpin + '_>> {
    let bytes = bytes?;
    let reader: Box<dyn AsyncRead + Send + Unpin> = Box::new(Cursor::new(bytes));
    Ok(reader)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::interchange::Json;
    use crate::repository::{fetch_metadata_to_string, fetch_target_to_string};
    use futures_executor::block_on;

    #[test]
    fn ephemeral_repo_targets() {
        block_on(async {
            let mut repo = EphemeralRepository::<Json>::new();

            let data: &[u8] = b"like tears in the rain";
            let path = TargetPath::new("batty").unwrap();
            repo.store_target(&path, &mut &*data).await.unwrap();

            let mut read = repo.fetch_target(&path).await.unwrap();
            let mut buf = Vec::new();
            read.read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf.as_slice(), data);
            drop(read);

            // RepositoryProvider implementations do not guarantee data is not corrupt.
            let bad_data: &[u8] = b"you're in a desert";
            repo.store_target(&path, &mut &*bad_data).await.unwrap();
            let mut read = repo.fetch_target(&path).await.unwrap();
            buf.clear();
            read.read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf.as_slice(), bad_data);
        })
    }

    #[test]
    fn ephemeral_repo_transaction() {
        block_on(async {
            let mut repo = EphemeralRepository::<Json>::new();

            let meta_path = MetadataPath::new("meta").unwrap();
            let meta_version = MetadataVersion::None;
            let target_path = TargetPath::new("target").unwrap();

            // First, write some stuff to the repository.
            let committed_meta = "committed meta";
            let committed_target = "committed target";

            repo.store_metadata(&meta_path, &meta_version, &mut committed_meta.as_bytes())
                .await
                .unwrap();

            repo.store_target(&target_path, &mut committed_target.as_bytes())
                .await
                .unwrap();

            let mut tx = repo.transaction();

            // Make sure we can read back the committed stuff.
            assert_eq!(
                fetch_metadata_to_string(&tx, &meta_path, &meta_version)
                    .await
                    .unwrap(),
                committed_meta,
            );
            assert_eq!(
                fetch_target_to_string(&tx, &target_path).await.unwrap(),
                committed_target,
            );

            // Next, stage some stuff in the transaction.
            let staged_meta = "staged meta";
            let staged_target = "staged target";
            tx.store_metadata(&meta_path, &meta_version, &mut staged_meta.as_bytes())
                .await
                .unwrap();
            tx.store_target(&target_path, &mut staged_target.as_bytes())
                .await
                .unwrap();

            // Make sure it got staged.
            assert_eq!(
                fetch_metadata_to_string(&tx, &meta_path, &meta_version)
                    .await
                    .unwrap(),
                staged_meta,
            );
            assert_eq!(
                fetch_target_to_string(&tx, &target_path).await.unwrap(),
                staged_target,
            );

            // Next, drop the transaction. We shouldn't have written the data back to the
            // repository.
            drop(tx);

            assert_eq!(
                fetch_metadata_to_string(&repo, &meta_path, &meta_version)
                    .await
                    .unwrap(),
                committed_meta,
            );
            assert_eq!(
                fetch_target_to_string(&repo, &target_path).await.unwrap(),
                committed_target,
            );

            // Do the transaction again, but this time commit the data.
            let mut tx = repo.transaction();
            tx.store_metadata(&meta_path, &meta_version, &mut staged_meta.as_bytes())
                .await
                .unwrap();
            tx.store_target(&target_path, &mut staged_target.as_bytes())
                .await
                .unwrap();
            tx.commit();

            // Make sure the new data got to the repository.
            assert_eq!(
                fetch_metadata_to_string(&repo, &meta_path, &meta_version)
                    .await
                    .unwrap(),
                staged_meta,
            );
            assert_eq!(
                fetch_target_to_string(&repo, &target_path).await.unwrap(),
                staged_target,
            );
        })
    }
}
