use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions, TryLockError},
    io,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerLoss {
    DropUnsynced,
    KeepUnsynced,
    Random(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimOp {
    Write,
    Sync,
    SetLen,
    SyncDir,
}

#[derive(Debug, Clone)]
enum Pending {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

#[derive(Debug, Default)]
struct SimFileState {
    durable: Vec<u8>,
    current: Vec<u8>,
    pending: Vec<Pending>,
}

fn apply(bytes: &mut Vec<u8>, op: &Pending) {
    match op {
        Pending::Write { offset, data } => write_into(bytes, *offset, data),
        Pending::SetLen(len) => bytes.resize(*len as usize, 0),
    }
}

fn write_into(bytes: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = offset as usize;
    let end = start + data.len();
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    bytes[start..end].copy_from_slice(data);
}

#[derive(Default)]
struct Faults {
    ops: u64,
    crash_at: Option<u64>,
    fail_at: BTreeSet<u64>,
    crashed: bool,
    trace: Vec<(u64, SimOp, PathBuf)>,
    tracing: bool,
}

struct SimShared {
    faults: Mutex<Faults>,
}

impl SimShared {
    fn faults(&self) -> MutexGuard<'_, Faults> {
        self.faults.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn check(&self, op: SimOp, path: &Path) -> io::Result<()> {
        let mut f = self.faults();
        if f.crashed {
            return Err(crashed_error());
        }
        f.ops += 1;
        let n = f.ops;
        if f.tracing {
            f.trace.push((n, op, path.to_path_buf()));
        }
        if f.crash_at == Some(n) {
            f.crashed = true;
            return Err(crashed_error());
        }
        if f.fail_at.remove(&n) {
            return Err(io::Error::other(format!(
                "injected {op:?} failure at operation {n} on {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn check_read(&self) -> io::Result<()> {
        if self.faults().crashed {
            return Err(crashed_error());
        }
        Ok(())
    }
}

fn crashed_error() -> io::Error {
    io::Error::other("simulated crash: the process is dead")
}

pub struct SimFile {
    path: PathBuf,
    state: Mutex<SimFileState>,
    shared: Arc<SimShared>,
}

impl SimFile {
    fn state(&self) -> MutexGuard<'_, SimFileState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn power_loss(&self, mode: PowerLoss, rng: &mut Rng) {
        let mut st = self.state();
        let mut bytes = st.durable.clone();
        let pending = std::mem::take(&mut st.pending);
        match mode {
            PowerLoss::DropUnsynced => {}
            PowerLoss::KeepUnsynced => {
                for op in &pending {
                    apply(&mut bytes, op);
                }
            }
            PowerLoss::Random(_) => {
                for op in &pending {
                    if rng.below(2) == 0 {
                        continue;
                    }
                    match op {
                        Pending::Write { offset, data }
                            if data.len() > 512 && rng.below(4) == 0 =>
                        {
                            let sectors = data.len().div_ceil(512);
                            let keep = rng.below(sectors as u64) as usize * 512;
                            write_into(&mut bytes, *offset, &data[..keep]);
                        }
                        other => apply(&mut bytes, other),
                    }
                }
            }
        }
        st.durable = bytes.clone();
        st.current = bytes;
    }
}

impl StorageFile for SimFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.shared.check_read()?;
        let st = self.state();
        let start = offset as usize;
        let end = start.checked_add(buf.len()).ok_or_else(eof)?;
        if end > st.current.len() {
            return Err(eof());
        }
        buf.copy_from_slice(&st.current[start..end]);
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.shared.check(SimOp::Write, &self.path)?;
        let mut st = self.state();
        write_into(&mut st.current, offset, buf);
        st.pending.push(Pending::Write {
            offset,
            data: buf.to_vec(),
        });
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let result = self.shared.check(SimOp::Sync, &self.path);
        let mut st = self.state();
        match result {
            Ok(()) => {
                let pending = std::mem::take(&mut st.pending);
                for op in &pending {
                    apply(&mut st.durable, op);
                }
                Ok(())
            }
            Err(e) => {
                if !self.shared.faults().crashed {
                    st.pending.clear();
                }
                Err(e)
            }
        }
    }

    fn len(&self) -> io::Result<u64> {
        self.shared.check_read()?;
        Ok(self.state().current.len() as u64)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.shared.check(SimOp::SetLen, &self.path)?;
        let mut st = self.state();
        st.current.resize(len as usize, 0);
        st.pending.push(Pending::SetLen(len));
        Ok(())
    }
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end of file")
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1) // WOO! magic
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

#[derive(Default)]
struct SimDir {
    files: BTreeMap<PathBuf, Arc<SimFile>>,
    unsynced_entries: BTreeSet<PathBuf>,
}

pub struct SimVfs {
    shared: Arc<SimShared>,
    dir: Mutex<SimDir>,
}

impl Default for SimVfs {
    fn default() -> Self {
        Self {
            shared: Arc::new(SimShared {
                faults: Mutex::new(Faults::default()),
            }),
            dir: Mutex::new(SimDir::default()),
        }
    }
}

impl SimVfs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn dir(&self) -> MutexGuard<'_, SimDir> {
        self.dir.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn op_count(&self) -> u64 {
        self.shared.faults().ops
    }

    pub fn crash_at(&self, op: u64) {
        self.shared.faults().crash_at = Some(op);
    }

    pub fn crash_after(&self, more_ops: u64) {
        let mut f = self.shared.faults();
        f.crash_at = Some(f.ops + more_ops);
    }

    pub fn fail_at(&self, op: u64) {
        self.shared.faults().fail_at.insert(op);
    }

    pub fn fail_after(&self, more_ops: u64) {
        let mut f = self.shared.faults();
        let n = f.ops + more_ops;
        f.fail_at.insert(n);
    }

    pub fn is_crashed(&self) -> bool {
        self.shared.faults().crashed
    }

    pub fn set_tracing(&self, on: bool) {
        let mut f = self.shared.faults();
        f.tracing = on;
        f.trace.clear();
    }

    pub fn trace(&self) -> Vec<(u64, SimOp, PathBuf)> {
        self.shared.faults().trace.clone()
    }

    pub fn power_loss(&self, mode: PowerLoss) {
        let mut rng = Rng::new(match mode {
            PowerLoss::Random(seed) => seed,
            _ => 1,
        });
        let mut dir = self.dir();
        let unsynced = std::mem::take(&mut dir.unsynced_entries);
        for path in unsynced {
            let vanish = match mode {
                PowerLoss::DropUnsynced => true,
                PowerLoss::KeepUnsynced => false,
                PowerLoss::Random(_) => rng.below(2) == 0,
            };
            if vanish {
                dir.files.remove(&path);
            }
        }
        for file in dir.files.values() {
            file.power_loss(mode, &mut rng);
        }
        let mut f = self.shared.faults();
        f.crashed = false;
        f.crash_at = None;
        f.fail_at.clear();
    }

    pub fn file_names(&self) -> Vec<PathBuf> {
        self.dir().files.keys().cloned().collect()
    }

    pub fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
        let dir = self.dir();
        let file = dir.files.get(path)?;
        Some(file.state().current.clone())
    }

    pub fn write_file(&self, path: &Path, bytes: Vec<u8>) {
        let mut dir = self.dir();
        let file = dir
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                Arc::new(SimFile {
                    path: path.to_path_buf(),
                    state: Mutex::new(SimFileState::default()),
                    shared: Arc::clone(&self.shared),
                })
            })
            .clone();
        let mut st = file.state();
        st.pending.clear();
        st.durable = bytes.clone();
        st.current = bytes;
    }
}

impl Vfs for SimVfs {
    fn open(&self, path: &Path) -> io::Result<FileRef> {
        self.shared.check_read()?;
        let mut dir = self.dir();
        if let Some(file) = dir.files.get(path) {
            return Ok(file.clone());
        }
        let file = Arc::new(SimFile {
            path: path.to_path_buf(),
            state: Mutex::new(SimFileState::default()),
            shared: Arc::clone(&self.shared),
        });
        dir.files.insert(path.to_path_buf(), Arc::clone(&file));
        dir.unsynced_entries.insert(path.to_path_buf());
        Ok(file)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.shared.check_read()?;
        Ok(self.dir().files.contains_key(path))
    }

    fn sync_dir(&self, dir_path: &Path) -> io::Result<()> {
        self.shared.check(SimOp::SyncDir, dir_path)?;
        let mut dir = self.dir();
        dir.unsynced_entries
            .retain(|p| p.parent().is_some_and(|parent| parent != dir_path));
        Ok(())
    }

    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        self.shared.check_read()
    }

    fn lock(&self, _path: &Path) -> io::Result<DirLock> {
        self.shared.check_read()?;
        Ok(DirLock { _file: None })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn sim_file(vfs: &SimVfs, name: &str) -> FileRef {
        vfs.open(Path::new(name)).unwrap()
    }

    #[test]
    fn synced_writes_survive_power_loss() {
        let vfs = SimVfs::new();
        let f = sim_file(&vfs, "/d/a");
        f.write_at(b"hello", 0).unwrap();
        f.sync().unwrap();
        vfs.sync_dir(Path::new("/d")).unwrap();
        f.write_at(b"HE", 0).unwrap();
        vfs.power_loss(PowerLoss::DropUnsynced);
        let f = sim_file(&vfs, "/d/a");
        let mut buf = [0u8; 5];
        f.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn unsynced_directory_entry_vanishes() {
        let vfs = SimVfs::new();
        let f = sim_file(&vfs, "/d/a");
        f.write_at(b"x", 0).unwrap();
        f.sync().unwrap();
        vfs.power_loss(PowerLoss::DropUnsynced);
        assert!(!vfs.exists(Path::new("/d/a")).unwrap());
    }

    #[test]
    fn keep_unsynced_keeps_everything() {
        let vfs = SimVfs::new();
        let f = sim_file(&vfs, "/d/a");
        f.write_at(b"abc", 0).unwrap();
        f.set_len(2).unwrap();
        vfs.power_loss(PowerLoss::KeepUnsynced);
        assert_eq!(vfs.read_file(Path::new("/d/a")).unwrap(), b"ab");
    }

    #[test]
    fn failed_sync_loses_the_pending_writes() {
        let vfs = SimVfs::new();
        let f = sim_file(&vfs, "/d/a");
        vfs.sync_dir(Path::new("/d")).unwrap();
        f.write_at(b"old", 0).unwrap();
        f.sync().unwrap();
        f.write_at(b"new", 0).unwrap();
        vfs.fail_after(1);
        assert!(f.sync().is_err());
        f.sync().unwrap();
        let mut buf = [0u8; 3];
        f.read_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"new");
        vfs.power_loss(PowerLoss::KeepUnsynced);
        assert_eq!(vfs.read_file(Path::new("/d/a")).unwrap(), b"old");
    }

    #[test]
    fn random_power_loss_is_deterministic_per_seed() {
        let run = |seed| {
            let vfs = SimVfs::new();
            let f = sim_file(&vfs, "/d/a");
            vfs.sync_dir(Path::new("/d")).unwrap();
            for i in 0..32u8 {
                f.write_at(&[i; 1024], i as u64 * 1024).unwrap();
            }
            vfs.power_loss(PowerLoss::Random(seed));
            vfs.read_file(Path::new("/d/a")).unwrap()
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }
}
