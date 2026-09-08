use std::{
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use crate::{
    FileOpener, FileSystem, FsError, Metadata, OpenOptions, OpenOptionsConfig, ReadDir, VirtualFile,
};

#[derive(Debug)]
struct Quota {
    used: AtomicU64,
    limit: u64,
}

impl Quota {
    fn reserve(&self, bytes: u64) -> crate::Result<()> {
        if bytes == 0 {
            return Ok(());
        }

        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|new_used| *new_used <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| FsError::StorageFull)
    }

    fn refund(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }

        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            })
            .expect("quota refund always produces a new counter value");
    }
}

/// Limits approximate net file growth initiated through a filesystem mount.
///
/// Existing contents are an uncounted baseline. Deleting or shrinking files
/// replenishes the budget without tracking whether their contents predated the
/// mount. Accounting keeps fixed state per mount and open handle without
/// scanning or retaining entries for the mounted tree. Mutations through other
/// handles or paths may over- or undercount growth. WASIX shares each inode's
/// handle and calls `VirtualFile::on_unlink` after namespace removal so that
/// subsequent shrinking cannot refund removed contents again. New growth of
/// an unlinked file remains charged until it is shrunk or the handle is dropped.
#[derive(Debug, Clone)]
pub struct QuotaFileSystem<F> {
    inner: F,
    quota: Arc<Quota>,
}

impl<F> QuotaFileSystem<F> {
    pub fn new(filesystem: F, maximum_growth_bytes: u64) -> Self {
        Self {
            inner: filesystem,
            quota: Arc::new(Quota {
                used: AtomicU64::new(0),
                limit: maximum_growth_bytes,
            }),
        }
    }

    #[cfg(test)]
    fn used(&self) -> u64 {
        self.quota.used.load(Ordering::Relaxed)
    }
}

impl<F> FileSystem for QuotaFileSystem<F>
where
    F: FileSystem,
{
    fn readlink(&self, path: &Path) -> crate::Result<PathBuf> {
        self.inner.readlink(path)
    }

    fn read_dir(&self, path: &Path) -> crate::Result<ReadDir> {
        self.inner.read_dir(path)
    }

    fn create_dir(&self, path: &Path) -> crate::Result<()> {
        self.inner.create_dir(path)
    }

    fn create_symlink(&self, source: &Path, target: &Path) -> crate::Result<()> {
        self.inner.create_symlink(source, target)
    }

    fn hard_link(&self, source: &Path, target: &Path) -> crate::Result<()> {
        self.inner.hard_link(source, target)
    }

    fn remove_dir(&self, path: &Path) -> crate::Result<()> {
        self.inner.remove_dir(path)
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(async move {
            let replaced_size = match self.inner.symlink_metadata(to) {
                Ok(metadata) if metadata.is_file() => metadata.len,
                Ok(_) | Err(FsError::EntryNotFound) => 0,
                Err(error) => return Err(error),
            };
            self.inner.rename(from, to).await?;
            if replaced_size != 0 {
                // A rename onto the same file (including hard links) leaves the source present.
                match self.inner.symlink_metadata(from) {
                    Err(FsError::EntryNotFound) => self.quota.refund(replaced_size),
                    Ok(_) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })
    }

    fn metadata(&self, path: &Path) -> crate::Result<Metadata> {
        self.inner.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> crate::Result<Metadata> {
        self.inner.symlink_metadata(path)
    }

    fn remove_file(&self, path: &Path) -> crate::Result<()> {
        let metadata = self.inner.symlink_metadata(path)?;
        self.inner.remove_file(path)?;
        if metadata.is_file() {
            self.quota.refund(metadata.len);
        }
        Ok(())
    }

    fn new_open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }
}

impl<F> FileOpener for QuotaFileSystem<F>
where
    F: FileSystem,
{
    fn open(
        &self,
        path: &Path,
        conf: &OpenOptionsConfig,
    ) -> crate::Result<Box<dyn VirtualFile + Send + Sync + 'static>> {
        let truncated_size = if conf.truncate && !conf.create_new {
            match self.inner.metadata(path) {
                Ok(metadata) => metadata.len,
                Err(FsError::EntryNotFound) if conf.create => 0,
                Err(error) => return Err(error),
            }
        } else {
            0
        };
        let file = self
            .inner
            .new_open_options()
            .options(conf.clone())
            .open(path)?;
        self.quota.refund(truncated_size);
        let known_size = if conf.truncate || conf.create_new {
            0
        } else {
            file.size()
        };
        Ok(Box::new(QuotaFile {
            file,
            quota: Arc::clone(&self.quota),
            position: 0,
            known_size,
            append: conf.append && !conf.truncate,
            unlinked_size: None,
        }))
    }
}

#[derive(Debug)]
struct QuotaFile {
    file: Box<dyn VirtualFile + Send + Sync + 'static>,
    quota: Arc<Quota>,
    position: u64,
    known_size: u64,
    append: bool,
    // The remaining prefix already refunded by unlink; later growth is still charged.
    unlinked_size: Option<u64>,
}

impl VirtualFile for QuotaFile {
    fn last_accessed(&self) -> u64 {
        self.file.last_accessed()
    }

    fn last_modified(&self) -> u64 {
        self.file.last_modified()
    }

    fn created_time(&self) -> u64 {
        self.file.created_time()
    }

    fn set_times(&mut self, atime: Option<u64>, mtime: Option<u64>) -> crate::Result<()> {
        self.file.set_times(atime, mtime)
    }

    fn size(&self) -> u64 {
        self.file.size()
    }

    fn set_len(&mut self, new_size: u64) -> crate::Result<()> {
        let old_size = self.known_size;
        let growth = new_size.saturating_sub(old_size);
        self.quota.reserve(growth)?;
        if let Err(error) = self.file.set_len(new_size) {
            self.quota.refund(growth);
            return Err(error);
        }
        let refundable_size = old_size.saturating_sub(self.unlinked_size.unwrap_or(0));
        self.quota
            .refund(old_size.saturating_sub(new_size).min(refundable_size));
        if let Some(unlinked_size) = &mut self.unlinked_size {
            *unlinked_size = (*unlinked_size).min(new_size);
        }
        self.known_size = new_size;
        Ok(())
    }

    fn unlink(&mut self) -> crate::Result<()> {
        self.file.unlink()?;
        self.quota.refund(
            self.known_size
                .saturating_sub(self.unlinked_size.unwrap_or(0)),
        );
        self.on_unlink();
        Ok(())
    }

    fn on_unlink(&mut self) {
        self.file.on_unlink();
        self.unlinked_size = Some(self.known_size);
    }

    fn is_open(&self) -> bool {
        self.file.is_open()
    }

    fn get_special_fd(&self) -> Option<u32> {
        self.file.get_special_fd()
    }

    fn poll_read_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.file).poll_read_ready(cx)
    }

    fn poll_write_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.file).poll_write_ready(cx)
    }
}

impl Drop for QuotaFile {
    fn drop(&mut self) {
        if let Some(unlinked_size) = self.unlinked_size {
            self.quota
                .refund(self.known_size.saturating_sub(unlinked_size));
        }
    }
}

impl AsyncRead for QuotaFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled_before = buf.filled().len();
        let result = Pin::new(&mut *self.file).poll_read(cx, buf);
        if result.is_ready() {
            self.position = self
                .position
                .saturating_add(buf.filled().len().saturating_sub(filled_before) as u64);
        }
        result
    }
}

impl AsyncWrite for QuotaFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let position = if self.append {
            self.known_size
        } else {
            self.position
        };
        let requested_end = position
            .checked_add(buf.len() as u64)
            .ok_or(FsError::StorageFull)?;
        let reserved = requested_end.saturating_sub(self.known_size);
        self.quota.reserve(reserved)?;

        let result = Pin::new(&mut *self.file).poll_write(cx, buf);
        match result {
            Poll::Ready(Ok(written)) if written != 0 => {
                let end = position + written as u64;
                let growth = end.saturating_sub(self.known_size);
                self.quota.refund(reserved.saturating_sub(growth));
                self.position = end;
                self.known_size = self.known_size.max(end);
            }
            _ => self.quota.refund(reserved),
        };
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.file).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.file).poll_shutdown(cx)
    }
}

impl AsyncSeek for QuotaFile {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        Pin::new(&mut *self.file).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        match Pin::new(&mut *self.file).poll_complete(cx) {
            Poll::Ready(Ok(position)) => {
                self.position = position;
                Poll::Ready(Ok(position))
            }
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TmpFileSystem;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    fn create_file(filesystem: &impl FileSystem, path: &str) -> Box<dyn VirtualFile + Send + Sync> {
        filesystem
            .new_open_options()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap()
    }

    #[tokio::test]
    async fn sparse_writes_charge_holes_but_empty_writes_do_not() {
        let filesystem = QuotaFileSystem::new(TmpFileSystem::new(), 4);
        let mut file = create_file(&filesystem, "/file");
        file.seek(io::SeekFrom::Start(8)).await.unwrap();
        assert_eq!(file.write(b"").await.unwrap(), 0);
        assert_eq!(filesystem.used(), 0);
        assert_eq!(file.size(), 0);
        assert_eq!(
            file.write(b"x").await.unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );

        file.seek(io::SeekFrom::Start(2)).await.unwrap();
        file.write_all(b"ab").await.unwrap();
        assert_eq!(filesystem.used(), 4);
        assert_eq!(file.size(), 4);
        file.seek(io::SeekFrom::Start(0)).await.unwrap();
        file.write_all(b"12").await.unwrap();
        assert_eq!(filesystem.used(), 4);
    }

    #[tokio::test]
    async fn limits_growth_without_counting_existing_files() {
        let inner = TmpFileSystem::new();
        let mut existing = create_file(&inner, "/existing");
        existing.write_all(b"baseline").await.unwrap();
        drop(existing);

        let filesystem = QuotaFileSystem::new(inner, 4);
        let mut existing = filesystem
            .new_open_options()
            .write(true)
            .open("/existing")
            .unwrap();
        existing.write_all(b"replace!").await.unwrap();
        assert_eq!(filesystem.used(), 0);

        let mut output = create_file(&filesystem, "/output");
        output.write_all(b"1234").await.unwrap();
        assert_eq!(filesystem.used(), 4);

        let error = output.write_all(b"5").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(filesystem.used(), 4);
    }

    #[tokio::test]
    async fn shrinking_replenishes_growth_budget() {
        let filesystem = QuotaFileSystem::new(TmpFileSystem::new(), 4);
        let mut first = create_file(&filesystem, "/first");
        first.write_all(b"1234").await.unwrap();
        first.set_len(2).unwrap();

        let mut second = create_file(&filesystem, "/second");
        second.write_all(b"12").await.unwrap();

        assert_eq!(filesystem.used(), 4);
    }

    #[tokio::test]
    async fn unlink_then_shrink_cannot_refund_retained_files() {
        let filesystem = QuotaFileSystem::new(TmpFileSystem::new(), 4);
        let mut removed = create_file(&filesystem, "/removed");
        removed.write_all(b"1234").await.unwrap();
        removed.unlink().unwrap();

        let mut retained = create_file(&filesystem, "/retained");
        retained.write_all(b"5678").await.unwrap();
        removed.set_len(0).unwrap();
        drop(removed);

        assert_eq!(filesystem.used(), 4);
        assert_eq!(
            retained.write_all(b"9").await.unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
    }

    #[tokio::test]
    async fn unlinked_handle_charges_new_growth_and_refunds_it_on_close() {
        let filesystem = QuotaFileSystem::new(TmpFileSystem::new(), 8);
        let mut file = create_file(&filesystem, "/removed");
        file.write_all(b"1234").await.unwrap();
        file.unlink().unwrap();
        file.write_all(b"5678").await.unwrap();
        assert_eq!(filesystem.used(), 4);
        file.set_len(2).unwrap();
        assert_eq!(filesystem.used(), 0);
        file.set_len(6).unwrap();
        assert_eq!(filesystem.used(), 4);
        drop(file);
        assert_eq!(filesystem.used(), 0);
    }

    #[tokio::test]
    async fn reopening_preserves_shrink_and_truncate_refunds() {
        let filesystem = QuotaFileSystem::new(TmpFileSystem::new(), 4);
        let mut file = create_file(&filesystem, "/file");
        file.write_all(b"1234").await.unwrap();
        drop(file);

        let mut file = filesystem
            .new_open_options()
            .write(true)
            .open("/file")
            .unwrap();
        file.set_len(2).unwrap();
        assert_eq!(filesystem.used(), 2);
        drop(file);
        let file = filesystem
            .new_open_options()
            .write(true)
            .truncate(true)
            .open("/file")
            .unwrap();
        assert_eq!(filesystem.used(), 0);
        assert_eq!(file.size(), 0);
    }

    #[cfg(feature = "host-fs")]
    #[tokio::test]
    async fn replacement_refunds_once_and_failed_or_noop_renames_do_not_refund() {
        let directory = tempfile::tempdir().unwrap();
        let inner =
            crate::host_fs::FileSystem::new(tokio::runtime::Handle::current(), directory.path())
                .unwrap();
        let filesystem = QuotaFileSystem::new(inner, 8);
        for path in ["/first", "/second"] {
            create_file(&filesystem, path).set_len(4).unwrap();
        }
        filesystem
            .rename(Path::new("/second"), Path::new("/first"))
            .await
            .unwrap();
        assert_eq!(filesystem.used(), 4);
        filesystem
            .rename(Path::new("/first"), Path::new("/./first"))
            .await
            .unwrap();
        assert_eq!(filesystem.used(), 4);
        assert_eq!(
            filesystem
                .rename(Path::new("/missing"), Path::new("/first"))
                .await,
            Err(FsError::EntryNotFound)
        );
        assert_eq!(filesystem.used(), 4);
        filesystem.remove_file(Path::new("/first")).unwrap();
        assert_eq!(filesystem.used(), 0);
    }

    #[cfg(all(feature = "host-fs", unix))]
    #[tokio::test]
    async fn removing_symlink_does_not_refund_target_contents() {
        let directory = tempfile::tempdir().unwrap();
        let inner =
            crate::host_fs::FileSystem::new(tokio::runtime::Handle::current(), directory.path())
                .unwrap();
        let filesystem = QuotaFileSystem::new(inner, 4);
        create_file(&filesystem, "/file").set_len(4).unwrap();
        std::os::unix::fs::symlink("file", directory.path().join("link")).unwrap();
        filesystem.remove_file(Path::new("/link")).unwrap();
        assert_eq!(filesystem.used(), 4);
        assert_eq!(filesystem.metadata(Path::new("/file")).unwrap().len, 4);
    }
}
