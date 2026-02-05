//! Asynchronous file system operations.
//!
//! This module provides async wrappers around `std::fs` operations using
//! `spawn_blocking` to run them on background threads.

use std::io;
use std::path::Path;

use crate::task::spawn_blocking;


// =============================================================================
// Utility functions
// =============================================================================

/// Reads the entire contents of a file into a bytes vector.
pub async fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let path = path.as_ref().to_owned();
    trace!("fs::read {:?}", path);
    spawn_blocking(move || std::fs::read(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Reads the entire contents of a file into a string.
pub async fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
    let path = path.as_ref().to_owned();
    trace!("fs::read_to_string {:?}", path);
    spawn_blocking(move || std::fs::read_to_string(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Writes a slice as the entire contents of a file.
pub async fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    let contents = contents.as_ref().to_owned();
    trace!("fs::write {:?}", path);
    spawn_blocking(move || std::fs::write(&path, &contents)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Copies the contents of one file to another.
pub async fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
    let from = from.as_ref().to_owned();
    let to = to.as_ref().to_owned();
    trace!("fs::copy {:?} -> {:?}", from, to);
    spawn_blocking(move || std::fs::copy(&from, &to)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Removes a file from the filesystem.
pub async fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    trace!("fs::remove_file {:?}", path);
    spawn_blocking(move || std::fs::remove_file(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Creates a new, empty directory at the provided path.
pub async fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    trace!("fs::create_dir {:?}", path);
    spawn_blocking(move || std::fs::create_dir(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Recursively creates a directory and all of its parent components.
pub async fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    trace!("fs::create_dir_all {:?}", path);
    spawn_blocking(move || std::fs::create_dir_all(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Removes an empty directory.
pub async fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    trace!("fs::remove_dir {:?}", path);
    spawn_blocking(move || std::fs::remove_dir(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Removes a directory at this path, after removing all its contents.
pub async fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    trace!("fs::remove_dir_all {:?}", path);
    spawn_blocking(move || std::fs::remove_dir_all(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Renames a file or directory.
pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    let from = from.as_ref().to_owned();
    let to = to.as_ref().to_owned();
    trace!("fs::rename {:?} -> {:?}", from, to);
    spawn_blocking(move || std::fs::rename(&from, &to)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Queries the metadata about a path.
pub async fn metadata<P: AsRef<Path>>(path: P) -> io::Result<std::fs::Metadata> {
    let path = path.as_ref().to_owned();
    spawn_blocking(move || std::fs::metadata(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Queries the metadata about a path without following symlinks.
pub async fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<std::fs::Metadata> {
    let path = path.as_ref().to_owned();
    spawn_blocking(move || std::fs::symlink_metadata(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Returns the canonical form of a path.
pub async fn canonicalize<P: AsRef<Path>>(path: P) -> io::Result<std::path::PathBuf> {
    let path = path.as_ref().to_owned();
    spawn_blocking(move || std::fs::canonicalize(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Creates a new hard link on the filesystem.
pub async fn hard_link<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) -> io::Result<()> {
    let src = src.as_ref().to_owned();
    let dst = dst.as_ref().to_owned();
    spawn_blocking(move || std::fs::hard_link(&src, &dst)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Reads a symbolic link, returning the path it points to.
pub async fn read_link<P: AsRef<Path>>(path: P) -> io::Result<std::path::PathBuf> {
    let path = path.as_ref().to_owned();
    spawn_blocking(move || std::fs::read_link(&path)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Creates a new symbolic link on the filesystem (Unix).
#[cfg(unix)]
pub async fn symlink<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) -> io::Result<()> {
    let src = src.as_ref().to_owned();
    let dst = dst.as_ref().to_owned();
    spawn_blocking(move || std::os::unix::fs::symlink(&src, &dst)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Changes the permissions of a file or directory.
pub async fn set_permissions<P: AsRef<Path>>(path: P, perm: std::fs::Permissions) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    spawn_blocking(move || std::fs::set_permissions(&path, perm)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

/// Returns whether the path exists.
pub async fn try_exists<P: AsRef<Path>>(path: P) -> io::Result<bool> {
    let path = path.as_ref().to_owned();
    spawn_blocking(move || path.try_exists()).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
}

// =============================================================================
// File
// =============================================================================

/// An async file handle.
pub struct File {
    std: Option<std::fs::File>,
    path: Option<std::path::PathBuf>,
}

impl File {
    /// Opens a file in read-only mode.
    pub async fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
        let path = path.as_ref().to_owned();
        trace!("File::open {:?}", path);
        let file = spawn_blocking({
            let path = path.clone();
            move || std::fs::File::open(&path)
        }).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))??;
        Ok(File { std: Some(file), path: Some(path) })
    }

    /// Opens a file in write-only mode, creating it if it doesn't exist.
    pub async fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
        let path = path.as_ref().to_owned();
        trace!("File::create {:?}", path);
        let file = spawn_blocking({
            let path = path.clone();
            move || std::fs::File::create(&path)
        }).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))??;
        Ok(File { std: Some(file), path: Some(path) })
    }

    /// Converts a std::fs::File into a tokio::fs::File.
    pub fn from_std(std: std::fs::File) -> File {
        File { std: Some(std), path: None }
    }

    /// Attempts to sync all OS-internal metadata to disk.
    pub async fn sync_all(&self) -> io::Result<()> {
        let file = self.std.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "file closed"))?;
        let file = file.try_clone()?;
        spawn_blocking(move || file.sync_all()).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
    }

    /// Similar to `sync_all`, but may not sync file metadata.
    pub async fn sync_data(&self) -> io::Result<()> {
        let file = self.std.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "file closed"))?;
        let file = file.try_clone()?;
        spawn_blocking(move || file.sync_data()).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
    }

    /// Truncates or extends the file to the specified size.
    pub async fn set_len(&self, size: u64) -> io::Result<()> {
        let file = self.std.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "file closed"))?;
        let file = file.try_clone()?;
        spawn_blocking(move || file.set_len(size)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
    }

    /// Queries metadata about the file.
    pub async fn metadata(&self) -> io::Result<std::fs::Metadata> {
        let file = self.std.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "file closed"))?;
        let file = file.try_clone()?;
        spawn_blocking(move || file.metadata()).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
    }

    /// Creates a new independently owned handle to the underlying file.
    pub async fn try_clone(&self) -> io::Result<File> {
        let file = self.std.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "file closed"))?;
        let file = file.try_clone()?;
        Ok(File { std: Some(file), path: self.path.clone() })
    }

    /// Destructures the file into its underlying std::fs::File.
    pub fn into_std(mut self) -> std::fs::File {
        self.std.take().expect("file already consumed")
    }

    /// Changes the permissions on the underlying file.
    pub async fn set_permissions(&self, perm: std::fs::Permissions) -> io::Result<()> {
        let file = self.std.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "file closed"))?;
        let file = file.try_clone()?;
        spawn_blocking(move || file.set_permissions(perm)).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("File");
        if let Some(path) = &self.path {
            d.field("path", path);
        }
        d.finish()
    }
}

// =============================================================================
// OpenOptions
// =============================================================================

/// Options for opening files.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    inner: std::fs::OpenOptions,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    /// Creates a blank set of options.
    pub fn new() -> OpenOptions {
        OpenOptions { inner: std::fs::OpenOptions::new() }
    }

    /// Sets the option for read access.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.inner.read(read);
        self
    }

    /// Sets the option for write access.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.inner.write(write);
        self
    }

    /// Sets the option for append mode.
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.inner.append(append);
        self
    }

    /// Sets the option for truncating a previous file.
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.inner.truncate(truncate);
        self
    }

    /// Sets the option for creating a new file.
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.inner.create(create);
        self
    }

    /// Sets the option for creating a new file, failing if it already exists.
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.inner.create_new(create_new);
        self
    }

    /// Opens a file with the options specified.
    pub async fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        let path = path.as_ref().to_owned();
        let opts = self.inner.clone();
        trace!("OpenOptions::open {:?}", path);
        let file = spawn_blocking({
            let path = path.clone();
            move || opts.open(&path)
        }).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))??;
        Ok(File { std: Some(file), path: Some(path) })
    }
}

// =============================================================================
// ReadDir
// =============================================================================

/// Reads the entries in a directory.
pub async fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let path = path.as_ref().to_owned();
    trace!("fs::read_dir {:?}", path);
    let inner = spawn_blocking(move || std::fs::read_dir(&path)).await
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))??;
    Ok(ReadDir { inner: Some(inner) })
}

/// A stream of entries in a directory.
pub struct ReadDir {
    inner: Option<std::fs::ReadDir>,
}

impl ReadDir {
    /// Returns the next entry in the directory.
    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        let mut inner = self.inner.take().ok_or_else(|| io::Error::new(io::ErrorKind::Other, "read_dir exhausted"))?;
        let result = spawn_blocking(move || {
            let entry = inner.next();
            (inner, entry)
        }).await.map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?;
        
        let (inner, entry) = result;
        self.inner = Some(inner);
        
        match entry {
            Some(Ok(e)) => Ok(Some(DirEntry { inner: e })),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

/// An entry inside a directory.
pub struct DirEntry {
    inner: std::fs::DirEntry,
}

impl DirEntry {
    /// Returns the full path to this entry.
    pub fn path(&self) -> std::path::PathBuf {
        self.inner.path()
    }

    /// Returns the file name of this entry.
    pub fn file_name(&self) -> std::ffi::OsString {
        self.inner.file_name()
    }

    /// Returns the metadata for this entry.
    pub async fn metadata(&self) -> io::Result<std::fs::Metadata> {
        let inner = self.inner.path();
        spawn_blocking(move || std::fs::metadata(&inner)).await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
    }

    /// Returns the file type for this entry.
    pub async fn file_type(&self) -> io::Result<std::fs::FileType> {
        let inner = self.inner.path();
        spawn_blocking(move || std::fs::metadata(&inner).map(|m| m.file_type())).await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "task join error"))?
    }
}

impl std::fmt::Debug for DirEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirEntry")
            .field("path", &self.path())
            .finish()
    }
}
