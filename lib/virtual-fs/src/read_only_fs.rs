use std::{
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use crate::{
    FileOpener, FileSystem, FsError, Metadata, OpenOptions, OpenOptionsConfig, ReadDir, VirtualFile,
};

/// A filesystem view that permits reads while rejecting every mutation.
#[derive(Debug, Clone)]
pub struct ReadOnlyFileSystem<F>(F);

impl<F> ReadOnlyFileSystem<F> {
    pub fn new(filesystem: F) -> Self {
        Self(filesystem)
    }
}

impl<F> FileSystem for ReadOnlyFileSystem<F>
where
    F: FileSystem,
{
    fn readlink(&self, path: &Path) -> crate::Result<PathBuf> {
        self.0.readlink(path)
    }

    fn read_dir(&self, path: &Path) -> crate::Result<ReadDir> {
        self.0.read_dir(path)
    }

    fn create_dir(&self, _path: &Path) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn create_symlink(&self, _source: &Path, _target: &Path) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn hard_link(&self, _source: &Path, _target: &Path) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn remove_dir(&self, _path: &Path) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn rename<'a>(&'a self, _from: &'a Path, _to: &'a Path) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(async { Err(FsError::ReadOnlyFilesystem) })
    }

    fn metadata(&self, path: &Path) -> crate::Result<Metadata> {
        self.0.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> crate::Result<Metadata> {
        self.0.symlink_metadata(path)
    }

    fn remove_file(&self, _path: &Path) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn new_open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }
}

impl<F> FileOpener for ReadOnlyFileSystem<F>
where
    F: FileSystem,
{
    fn open(
        &self,
        path: &Path,
        conf: &OpenOptionsConfig,
    ) -> crate::Result<Box<dyn VirtualFile + Send + Sync + 'static>> {
        if conf.would_mutate() {
            return Err(FsError::ReadOnlyFilesystem);
        }

        let file = self.0.new_open_options().options(conf.clone()).open(path)?;
        Ok(Box::new(ReadOnlyFile(file)))
    }
}

#[derive(Debug)]
struct ReadOnlyFile(Box<dyn VirtualFile + Send + Sync + 'static>);

impl VirtualFile for ReadOnlyFile {
    fn last_accessed(&self) -> u64 {
        self.0.last_accessed()
    }

    fn last_modified(&self) -> u64 {
        self.0.last_modified()
    }

    fn created_time(&self) -> u64 {
        self.0.created_time()
    }

    fn set_times(&mut self, _atime: Option<u64>, _mtime: Option<u64>) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn size(&self) -> u64 {
        self.0.size()
    }

    fn set_len(&mut self, _new_size: u64) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn unlink(&mut self) -> crate::Result<()> {
        Err(FsError::ReadOnlyFilesystem)
    }

    fn on_unlink(&mut self) {
        self.0.on_unlink();
    }

    fn is_open(&self) -> bool {
        self.0.is_open()
    }

    fn get_special_fd(&self) -> Option<u32> {
        self.0.get_special_fd()
    }

    fn poll_read_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_read_ready(cx)
    }

    fn poll_write_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(FsError::ReadOnlyFilesystem.into()))
    }
}

impl AsyncRead for ReadOnlyFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for ReadOnlyFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(FsError::ReadOnlyFilesystem.into()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

impl AsyncSeek for ReadOnlyFile {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        Pin::new(&mut *self.0).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Pin::new(&mut *self.0).poll_complete(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TmpFileSystem;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn permits_reads_and_rejects_mutations() {
        let filesystem = TmpFileSystem::new();
        let mut input = filesystem
            .new_open_options()
            .write(true)
            .create_new(true)
            .open("/input.txt")
            .unwrap();
        input.write_all(b"input").await.unwrap();
        drop(input);
        let filesystem = ReadOnlyFileSystem::new(filesystem);

        let mut file = filesystem
            .new_open_options()
            .read(true)
            .open("/input.txt")
            .unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).await.unwrap();

        assert_eq!(contents, b"input");
        assert_eq!(
            filesystem.create_dir(Path::new("/new")),
            Err(FsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            filesystem
                .new_open_options()
                .write(true)
                .open("/input.txt")
                .unwrap_err(),
            FsError::ReadOnlyFilesystem
        );
        assert_eq!(file.set_len(1), Err(FsError::ReadOnlyFilesystem));
        assert_eq!(file.unlink(), Err(FsError::ReadOnlyFilesystem));
    }
}
