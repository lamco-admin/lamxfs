//! Path → inode-number resolution.
//!
//! Walks path components from the root, reading each intermediate directory and
//! looking up the next component. Symlinks are **not** followed — neither at
//! intermediate components nor the leaf — matching the `lambutter` contract; a
//! consumer that wants POSIX `stat` semantics follows symlinks itself via
//! `read_link`. (Stock RHEL-family `/boot` never uses symlinks in the path.)

use crate::{
    block_read::BlockRead,
    dir,
    error::{Error, Result},
    inode::Dinode,
    path::Path,
    superblock::Superblock,
};

/// Resolve `path` to an inode number, starting at the root.
pub(crate) fn resolve<R: BlockRead>(
    reader: &mut R,
    sb: &Superblock,
    path: Path<'_>,
) -> Result<u64> {
    let mut ino = sb.rootino;
    for comp in path.components() {
        if comp == b".." {
            // Parent traversal is not needed for boot paths and would require
            // tracking the walk stack; refuse rather than mis-resolve.
            return Err(Error::NotFound {
                component: "parent_dir_unsupported",
            });
        }
        let dir_inode = Dinode::read(reader, sb, ino)?;
        if !dir_inode.is_dir() {
            return Err(Error::NotFound {
                component: "not_a_directory",
            });
        }
        let entries = dir::read_dir(reader, sb, &dir_inode)?;
        let next = entries
            .iter()
            .find(|e| e.name == comp)
            .ok_or(Error::NotFound {
                component: "no_such_entry",
            })?;
        ino = next.inumber;
    }
    Ok(ino)
}
