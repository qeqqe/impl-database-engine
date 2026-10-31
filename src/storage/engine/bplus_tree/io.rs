use std::{
    fs::{File, OpenOptions, TryLockError},
    io,
    os::unix::fs::FileExt,
    path::Path,
    sync::Arc,
};

pub trait StorageFile: Send + Sync {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn set_len(&self, len: u64) -> io::Result<()>;

    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

pub type FileRef = Arc<dyn StorageFile>;

pub trait Vfs: Send + Sync {
    fn open(&self, path: &Path) -> io::Result<FileRef>;
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    fn lock(&self, path: &Path) -> io::Result<DirLock>;

    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => self.sync_dir(p),
            _ => self.sync_dir(Path::new(".")),
        }
    }
}

pub struct DirLock {
    _file: Option<File>,
}

pub struct OsFile {
    file: File,
}

impl OsFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self { file })
    }
}

impl StorageFile for OsFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(buf, offset)
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OsVfs;

impl OsVfs {
    pub fn shared() -> Arc<dyn Vfs> {
        Arc::new(OsVfs)
    }
}

impl Vfs for OsVfs {
    fn open(&self, path: &Path) -> io::Result<FileRef> {
        Ok(Arc::new(OsFile::open(path)?))
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn lock(&self, path: &Path) -> io::Result<DirLock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(DirLock { _file: Some(file) }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is locked by another process", path.display()),
            )),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}
