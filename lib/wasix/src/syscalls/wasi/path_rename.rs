use std::path::PathBuf;

use anyhow::Context;

use super::*;
use crate::syscalls::*;

/// ### `path_rename()`
/// Rename a file or directory
/// Inputs:
/// - `Fd old_fd`
///     The base directory for `old_path`
/// - `const char* old_path`
///     Pointer to UTF8 bytes, the file to be renamed
/// - `u32 old_path_len`
///     The number of bytes to read from `old_path`
/// - `Fd new_fd`
///     The base directory for `new_path`
/// - `const char* new_path`
///     Pointer to UTF8 bytes, the new file name
/// - `u32 new_path_len`
///     The number of bytes to read from `new_path`
#[instrument(level = "trace", skip_all, fields(%old_fd, %new_fd, old_path = field::Empty, new_path = field::Empty), ret)]
pub fn path_rename<M: MemorySize>(
    mut ctx: FunctionEnvMut<'_, WasiEnv>,
    old_fd: WasiFd,
    old_path: WasmPtr<u8, M>,
    old_path_len: M::Offset,
    new_fd: WasiFd,
    new_path: WasmPtr<u8, M>,
    new_path_len: M::Offset,
) -> Result<Errno, WasiError> {
    WasiEnv::do_pending_operations(&mut ctx)?;

    let env = ctx.data();
    let (memory, mut state, inodes) = unsafe { env.get_memory_and_wasi_state_and_inodes(&ctx, 0) };
    let source_str = unsafe { get_input_str_ok!(&memory, old_path, old_path_len) };
    Span::current().record("old_path", source_str.as_str());
    let target_str = unsafe { get_input_str_ok!(&memory, new_path, new_path_len) };
    Span::current().record("new_path", target_str.as_str());

    let ret = path_rename_internal(&mut ctx, old_fd, &source_str, new_fd, &target_str)?;
    let env = ctx.data();

    if ret == Errno::Success {
        #[cfg(feature = "journal")]
        if env.enable_journal {
            JournalEffector::save_path_rename(&mut ctx, old_fd, source_str, new_fd, target_str)
                .map_err(|err| {
                    tracing::error!("failed to save path rename event - {}", err);
                    WasiError::Exit(ExitCode::from(Errno::Fault))
                })?;
        }
    }
    Ok(ret)
}

pub fn path_rename_internal(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    source_fd: WasiFd,
    source_path: &str,
    target_fd: WasiFd,
    target_path: &str,
) -> Result<Errno, WasiError> {
    let env = ctx.data();
    let (memory, mut state, inodes) = unsafe { env.get_memory_and_wasi_state_and_inodes(&ctx, 0) };

    {
        let source_fd = wasi_try_ok!(state.fs.get_fd(source_fd));
        if !source_fd.inner.rights.contains(Rights::PATH_RENAME_SOURCE) {
            return Ok(Errno::Access);
        }
        let target_fd = wasi_try_ok!(state.fs.get_fd(target_fd));
        if !target_fd.inner.rights.contains(Rights::PATH_RENAME_TARGET) {
            return Ok(Errno::Access);
        }
    }

    let source_inode = wasi_try_ok!(state.fs.get_inode_at_path(
        inodes,
        source_fd,
        source_path,
        false
    ));
    let target_inode = match state
        .fs
        .get_inode_at_path(inodes, target_fd, target_path, false)
    {
        Ok(inode) => Some(inode),
        Err(Errno::Noent) => None,
        Err(error) => return Ok(error),
    };
    let (source_parent_inode, source_entry_name) = wasi_try_ok!(state.fs.get_parent_inode_at_path(
        inodes,
        source_fd,
        Path::new(source_path),
        true
    ));
    let (target_parent_inode, target_entry_name) = wasi_try_ok!(state.fs.get_parent_inode_at_path(
        inodes,
        target_fd,
        Path::new(target_path),
        true
    ));
    let source_guest_path = {
        let guard = source_parent_inode.read();
        match guard.deref() {
            Kind::Dir { path, .. } => crate::fs::PosixPath::from_path(path)
                .join(&crate::fs::PosixPath::new(&source_entry_name))
                .into_path_buf(),
            Kind::Root { .. } => return Ok(Errno::Notcapable),
            _ => return Ok(Errno::Notdir),
        }
    };
    let target_guest_path = {
        let guard = target_parent_inode.read();
        match guard.deref() {
            Kind::Dir { path, .. } => crate::fs::PosixPath::from_path(path)
                .join(&crate::fs::PosixPath::new(&target_entry_name))
                .into_path_buf(),
            Kind::Root { .. } => return Ok(Errno::Notcapable),
            _ => return Ok(Errno::Notdir),
        }
    };
    if target_inode
        .as_ref()
        .is_some_and(|target| std::ptr::eq(target.deref(), source_inode.deref()))
    {
        return Ok(Errno::Success);
    }

    // Keep new opens and handle replacement outside the namespace transition.
    let _fd_map = state.fs.fd_map.write().unwrap();
    let (source_is_dir, has_backing_entry) = {
        let guard = source_inode.read();
        (
            matches!(guard.deref(), Kind::Dir { .. }),
            matches!(
                guard.deref(),
                Kind::File { .. } | Kind::Dir { .. } | Kind::Symlink { .. }
            ),
        )
    };
    if source_is_dir
        && crate::fs::PosixPath::from_path(&target_guest_path)
            .strip_prefix(&crate::fs::PosixPath::from_path(&source_guest_path))
            .is_some()
    {
        return Ok(Errno::Inval);
    }
    let is_ephemeral = state.fs.ephemeral_symlink_at(&source_guest_path).is_some();
    wasi_try_ok!(__asyncify_light(env, None, source_inode.flush())?);
    if let Some(target) = &target_inode {
        wasi_try_ok!(__asyncify_light(env, None, target.flush())?);
    }
    if has_backing_entry {
        let result = __asyncify_light(
            env,
            None,
            state.fs_rename(&source_guest_path, &target_guest_path),
        )?;
        match result {
            Ok(()) => {}
            Err(Errno::Noent) if is_ephemeral => {}
            Err(error) => return Ok(error),
        }
        // Native rename onto another hard link to the same file is a no-op.
        if !is_ephemeral && target_inode.is_some() {
            match state.fs.root_fs.symlink_metadata(&source_guest_path) {
                Ok(_) => return Ok(Errno::Success),
                Err(FsError::EntryNotFound) => {}
                Err(error) => return Ok(fs_error_into_wasi_err(error)),
            }
        }
    }

    {
        let mut guard = source_parent_inode.write();
        if let Kind::Dir { entries, .. } = guard.deref_mut() {
            entries.remove(&source_entry_name);
        }
    }
    {
        let mut guard = target_parent_inode.write();
        if let Kind::Dir { entries, .. } = guard.deref_mut() {
            entries.insert(target_entry_name.clone(), source_inode.clone());
        }
    }
    if let Some(target) = target_inode {
        target.on_unlink();
    }

    rename_inode_tree(&source_inode, &source_guest_path, &target_guest_path);
    *source_inode.name.write().unwrap() = target_entry_name.into();
    {
        let mut guard = source_inode.write();
        if let Kind::Symlink {
            path_to_symlink,
            relative_path,
            ..
        } = guard.deref_mut()
        {
            let new_path = state.fs.rebase_symlink_location(&target_guest_path);
            *path_to_symlink = new_path.clone();
            if is_ephemeral {
                state.fs.move_ephemeral_symlink(
                    &source_guest_path,
                    &target_guest_path,
                    new_path,
                    relative_path.clone(),
                );
            }
        }
    }
    if !is_ephemeral {
        state.fs.unregister_ephemeral_symlink(&target_guest_path);
    }
    Ok(Errno::Success)
}

fn rename_inode_tree(inode: &InodeGuard, source_dir_path: &Path, target_dir_path: &Path) {
    let children;

    let mut guard = inode.write();
    match guard.deref_mut() {
        Kind::File { path, .. } => {
            *path = adjust_path(path, source_dir_path, target_dir_path);
            return;
        }
        Kind::Dir { path, entries, .. } => {
            *path = adjust_path(path, source_dir_path, target_dir_path);
            children = entries.values().cloned().collect::<Vec<_>>();
        }
        _ => return,
    }
    drop(guard);

    for child in children {
        rename_inode_tree(&child, source_dir_path, target_dir_path);
    }
}

fn adjust_path(path: &Path, source_dir_path: &Path, target_dir_path: &Path) -> PathBuf {
    let path = crate::fs::PosixPath::from_path(path);
    let source_dir_path = crate::fs::PosixPath::from_path(source_dir_path);
    let relative_path = path
        .strip_prefix(&source_dir_path)
        .with_context(|| format!("Expected path {path:?} to be a subpath of {source_dir_path:?}"))
        .expect("Fatal filesystem error");
    crate::fs::PosixPath::from_path(target_dir_path)
        .join(&relative_path)
        .into_path_buf()
}
