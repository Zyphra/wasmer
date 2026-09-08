use super::*;
use crate::syscalls::*;

/// ### `path_unlink_file()`
/// Unlink a file, deleting if the number of hardlinks is 1
/// Inputs:
/// - `Fd fd`
///     The base file descriptor from which the path is understood
/// - `const char *path`
///     Array of UTF-8 bytes representing the path
/// - `u32 path_len`
///     The number of bytes in the `path` array
#[instrument(level = "trace", skip_all, fields(%fd, path = field::Empty), ret)]
pub fn path_unlink_file<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    fd: WasiFd,
    path: WasmPtr<u8, M>,
    path_len: M::Offset,
) -> Result<Errno, WasiError> {
    WasiEnv::do_pending_operations(&mut ctx)?;

    let env = ctx.data();
    let (memory, mut state, inodes) = unsafe { env.get_memory_and_wasi_state_and_inodes(&ctx, 0) };

    let base_dir = wasi_try_ok!(state.fs.get_fd(fd));
    if !base_dir.inner.rights.contains(Rights::PATH_UNLINK_FILE) {
        return Ok(Errno::Access);
    }
    let path_str = unsafe { get_input_str_ok!(&memory, path, path_len) };
    Span::current().record("path", path_str.as_str());

    let ret = path_unlink_file_internal(&mut ctx, fd, &path_str)?;
    let env = ctx.data();

    if ret == Errno::Success {
        #[cfg(feature = "journal")]
        if env.enable_journal {
            wasi_try_ok!(
                JournalEffector::save_path_unlink(&mut ctx, fd, path_str).map_err(|err| {
                    tracing::error!("failed to save unlink event - {}", err);
                    Errno::Fault
                })
            )
        }
    }

    Ok(ret)
}

pub(crate) fn path_unlink_file_internal(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    fd: WasiFd,
    path: &str,
) -> Result<Errno, WasiError> {
    let env = ctx.data();
    let (memory, mut state, inodes) = unsafe { env.get_memory_and_wasi_state_and_inodes(&ctx, 0) };

    let inode = wasi_try_ok!(state.fs.get_inode_at_path(inodes, fd, path, false));
    let (parent_inode, child_name) = wasi_try_ok!(state.fs.get_parent_inode_at_path(
        inodes,
        fd,
        std::path::Path::new(path),
        false
    ));
    let host_adjusted_path = {
        let guard = parent_inode.read();
        match guard.deref() {
            Kind::Dir { path, .. } => path.join(&child_name),
            Kind::Root { .. } => return Ok(Errno::Access),
            _ => unreachable!(
                "Internal logic error in wasi::path_unlink_file, parent is not a directory"
            ),
        }
    };

    // Serialize namespace removal with opening and replacing shared handles.
    let _fd_map = state.fs.fd_map.write().unwrap();
    let entry_ino = {
        let guard = parent_inode.read();
        match guard.deref() {
            Kind::Dir { entries, .. } => entries.get(&child_name).map(InodeGuard::ino),
            Kind::Root { .. } => return Ok(Errno::Access),
            _ => unreachable!(
                "Internal logic error in wasi::path_unlink_file, parent is not a directory"
            ),
        }
    };
    let Some(entry_ino) = entry_ino else {
        let inode_is_symlink = matches!(inode.read().deref(), Kind::Symlink { .. });
        if !inode_is_symlink {
            tracing::warn!(
                "wasi::path_unlink_file: path resolution returned inode {:?} for {:?}, but parent directory had no matching entry",
                inode.ino(),
                child_name
            );
            return Ok(Errno::Noent);
        }
        return Ok(state.fs.remove_symlink_file(host_adjusted_path.as_path()));
    };
    // TODO: make this a debug assert in the future
    assert!(inode.ino() == entry_ino);
    let (backing_path, is_symlink) = {
        let guard = inode.read();
        match guard.deref() {
            Kind::File { path, .. } => (path.clone(), false),
            Kind::Symlink { .. } => (host_adjusted_path, true),
            Kind::Dir { .. } | Kind::Root { .. } => return Ok(Errno::Isdir),
            _ => return Ok(Errno::Inval),
        }
    };
    debug_assert!(inode.stat.read().unwrap().st_nlink > 0);
    if inode.stat.read().unwrap().st_nlink == 1 {
        if is_symlink {
            let errno = state.fs.remove_symlink_file(backing_path.as_path());
            if errno != Errno::Success {
                return Ok(errno);
            }
        } else {
            wasi_try_ok!(__asyncify_light(env, None, inode.flush())?);
            wasi_try_ok!(state.fs_remove_file(&backing_path));
        }
    }
    {
        let mut guard = parent_inode.write();
        if let Kind::Dir { entries, .. } = guard.deref_mut() {
            entries.remove(&child_name);
        }
    }
    inode.on_unlink();
    Ok(Errno::Success)
}
