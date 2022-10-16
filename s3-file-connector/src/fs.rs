use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::prelude::OsStrExt;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, UNIX_EPOCH};
use tracing::{error, trace};

use fuser::{FileAttr, FileType, KernelConfig};
use s3_client::ObjectClient;

use crate::inode::{InodeError, InodeNo, InodeStat, InodeStatKind, ReaddirHandle, Superblock};
use crate::prefetch::{PrefetchGetObject, Prefetcher};

// FIXME Use newtype here? Will add a bunch of .into()s...
pub type Inode = u64;

pub const FUSE_ROOT_INODE: Inode = 1u64;

const DIR_PERMISSIONS: u16 = 0o755;
const FILE_PERMISSIONS: u16 = 0o644;
const UID: u32 = 501;
const GID: u32 = 20;

const BLOCK_SIZE: u64 = 4096;

#[derive(Debug)]
struct DirHandle {
    #[allow(unused)]
    ino: InodeNo,
    handle: ReaddirHandle,
    offset: AtomicI64,
}

impl DirHandle {
    fn offset(&self) -> i64 {
        self.offset.load(Ordering::SeqCst)
    }

    fn next_offset(&self) {
        self.offset.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct FileHandle<Client: ObjectClient> {
    #[allow(unused)]
    ino: InodeNo,
    full_key: OsString,
    object_size: u64,
    request: Mutex<Option<PrefetchGetObject<Client>>>,
}

pub struct S3FilesystemConfig {
    pub ttl_zero: Duration,
    pub readdir_size: usize,
}

impl Default for S3FilesystemConfig {
    fn default() -> Self {
        Self {
            ttl_zero: Duration::from_secs(0),
            readdir_size: 100,
        }
    }
}

pub struct S3Filesystem<Client: ObjectClient> {
    config: S3FilesystemConfig,
    client: Arc<Client>,
    superblock: Superblock,
    streaming_get_manager: Prefetcher<Client>,
    bucket: String,
    #[allow(unused)]
    prefix: String,
    next_handle: AtomicU64,
    dir_handles: RwLock<HashMap<u64, Arc<DirHandle>>>,
    file_handles: RwLock<HashMap<u64, FileHandle<Client>>>,
}

impl<Client: ObjectClient + Send + Sync + 'static> S3Filesystem<Client> {
    pub fn new(client: Client, bucket: &str, prefix: &str, config: S3FilesystemConfig) -> Self {
        // TODO is this required?
        assert!(
            prefix.is_empty() || prefix.ends_with('/'),
            "prefix must be empty or end with `/`"
        );

        let superblock = Superblock::new(bucket.to_string(), OsString::from(prefix));

        let client = Arc::new(client);

        let streaming_get_manager = Prefetcher::new(client.clone());

        Self {
            config,
            client,
            superblock,
            streaming_get_manager,
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
            next_handle: AtomicU64::new(1),
            dir_handles: RwLock::new(HashMap::new()),
            file_handles: RwLock::new(HashMap::new()),
        }
    }

    fn next_handle(&self) -> u64 {
        self.next_handle.fetch_add(1, Ordering::SeqCst)
    }
}

fn make_attr(ino: Inode, stat: &InodeStat) -> FileAttr {
    let (perm, nlink, blksize) = match stat.kind {
        InodeStatKind::File {} => (FILE_PERMISSIONS, 1, BLOCK_SIZE as u32),
        InodeStatKind::Directory {} => (DIR_PERMISSIONS, 2, 512),
    };
    FileAttr {
        ino,
        size: stat.size as u64,
        blocks: stat.size as u64 / BLOCK_SIZE,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH, // TODO
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: (&stat.kind).into(),
        perm,
        nlink,
        uid: UID,
        gid: GID,
        rdev: 0,
        flags: 0,
        blksize,
    }
}

/// Reply to a `lookup` call
pub struct Entry {
    pub ttl: Duration,
    pub attr: FileAttr,
    pub generation: u64,
}

/// Reply to a `getattr` call
pub struct Attr {
    pub ttl: Duration,
    pub attr: FileAttr,
}

/// Reply to a `open` or `opendir` call
pub struct Opened {
    pub fh: u64,
    pub flags: u32,
}

/// Reply to a `readdir` call
pub trait DirectoryReplier {
    /// Add a new dentry to the reply. Returns true if the buffer was full.
    fn add<T: AsRef<OsStr>>(&mut self, ino: u64, offset: i64, kind: FileType, name: T) -> bool;
}

/// Reply to a `read` call. This is funky because we want the reply to happen with only a borrow of
/// the bytes. But that borrow probably comes from some lock in this module or below, and we don't
/// want to have to shoehorn that lifetime into the layer above us. So instead we have this trait
/// that forces the `read` method to invoke exactly one of the reply methods. The idea is that the
/// [Replied] type should be private and unconstructable by this module.
pub trait ReadReplier {
    type Replied;
    /// Reply with a data payload
    fn data(self, data: &[u8]) -> Self::Replied;
    /// Reply with an error
    fn error(self, error: libc::c_int) -> Self::Replied;
}

impl<Client: ObjectClient + Send + Sync + 'static> S3Filesystem<Client> {
    pub async fn init(&self, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        let _ = config.set_max_readahead(0);
        Ok(())
    }

    pub async fn lookup(&self, parent: Inode, name: &OsStr) -> Result<Entry, libc::c_int> {
        trace!("fs:lookup with parent {:?} name {:?}", parent, name);

        let stat = self.superblock.lookup(&self.client, parent, name).await?;

        Ok(Entry {
            ttl: self.config.ttl_zero,
            attr: make_attr(stat.ino, &stat.stat),
            generation: 0,
        })
    }

    pub async fn getattr(&self, ino: Inode) -> Result<Attr, libc::c_int> {
        trace!("fs:getattr with ino {:?}", ino);

        let lookup = self.superblock.getattr(&self.client, ino).await?;

        Ok(Attr {
            ttl: self.config.ttl_zero,
            attr: make_attr(ino, &lookup.stat),
        })
    }

    pub async fn open(&self, ino: Inode, _flags: i32) -> Result<Opened, libc::c_int> {
        trace!("fs:open with ino {:?} flags {:?}", ino, _flags);

        let lookup = self.superblock.getattr(&self.client, ino).await?;

        // TODO validation:
        // - must be a file
        // - must be read-only flags, or file was created locally and is new

        let fh = self.next_handle();
        let handle = FileHandle {
            ino,
            full_key: lookup.full_key,
            object_size: lookup.stat.size as u64,
            request: Default::default(),
        };
        self.file_handles.write().unwrap().insert(fh, handle);

        Ok(Opened { fh, flags: 0 })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn read<R: ReadReplier>(
        &self,
        ino: Inode,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: R,
    ) -> R::Replied {
        trace!(
            "fs:read with ino {:?} fh {:?} offset {:?} size {:?}",
            ino,
            fh,
            offset,
            size
        );

        let file_handles = self.file_handles.read().unwrap();
        if let Some(handle) = file_handles.get(&fh) {
            let mut request = handle.request.lock().unwrap();
            if request.is_none() {
                let key = std::str::from_utf8(handle.full_key.as_bytes()).unwrap();
                *request = Some(self.streaming_get_manager.get(&self.bucket, key, handle.object_size));
            }
            let body = request.as_mut().unwrap().read(offset as u64, size as usize);
            reply.data(&body)
        } else {
            reply.error(libc::EBADF)
        }
    }

    pub async fn opendir(&self, parent: Inode, _flags: i32) -> Result<Opened, libc::c_int> {
        trace!("fs:opendir with parent {:?} flags {:?}", parent, _flags);

        let inode_handle = self.superblock.readdir(&self.client, parent, 1000).await?;

        let fh = self.next_handle();
        let handle = DirHandle {
            ino: parent,
            handle: inode_handle,
            offset: AtomicI64::new(0),
        };

        let mut dir_handles = self.dir_handles.write().unwrap();
        dir_handles.insert(fh, Arc::new(handle));

        Ok(Opened { fh, flags: 0 })
    }

    pub async fn readdir<R: DirectoryReplier>(
        &self,
        parent: Inode,
        fh: u64,
        offset: i64,
        mut reply: R,
    ) -> Result<R, libc::c_int> {
        trace!("fs:readdir with ino {:?} fh {:?} offset {:?}", parent, fh, offset);

        let handle = {
            let dir_handles = self.dir_handles.read().unwrap();
            dir_handles.get(&fh).cloned().ok_or(libc::EBADF)?
        };

        if offset != handle.offset() {
            error!(
                expected = handle.offset(),
                actual = offset,
                "fs:readdir: offset mismatch"
            );
            return Err(libc::EINVAL);
        }

        if handle.offset() < 1 {
            if reply.add(parent, handle.offset() + 1, FileType::Directory, ".") {
                return Ok(reply);
            }
            handle.next_offset();
        }
        if handle.offset() < 2 {
            if reply.add(handle.handle.parent(), handle.offset() + 1, FileType::Directory, "..") {
                return Ok(reply);
            }
            handle.next_offset();
        }

        loop {
            let next = match handle.handle.next(&self.client).await? {
                None => return Ok(reply),
                Some(next) => next,
            };

            if reply.add(
                next.ino,
                handle.offset() + 1,
                (&next.stat.kind).into(),
                next.name.clone(),
            ) {
                handle.handle.readd(next);
                return Ok(reply);
            }
            handle.next_offset();
        }
    }

    pub async fn release(
        &self,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
    ) -> Result<(), libc::c_int> {
        // TODO how do we cancel an inflight PrefetchingGetRequest?
        let mut file_handles = self.file_handles.write().unwrap();
        let existed = file_handles.remove(&fh).is_some();
        assert!(existed, "releasing a file handle that doesn't exist?");
        Ok(())
    }
}

impl From<InodeError> for i32 {
    fn from(err: InodeError) -> Self {
        match err {
            InodeError::ClientError(_) => libc::EIO,
            InodeError::FileDoesNotExist => libc::ENOENT,
            InodeError::InodeDoesNotExist(_) => libc::ENOENT,
            InodeError::InvalidFileName(_) => libc::EINVAL,
            InodeError::NotADirectory(_) => libc::ENOTDIR,
        }
    }
}
