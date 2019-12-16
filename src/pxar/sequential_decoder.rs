//! *pxar* format decoder.
//!
//! This module contain the code to decode *pxar* archive files.
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use endian_trait::Endian;
use failure::{bail, format_err, Error};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use nix::NixPath;

use proxmox::tools::io::ReadExt;
use proxmox::tools::vec;

use super::dir_stack::{PxarDir, PxarDirStack};
use super::flags;
use super::format_definition::*;
use super::match_pattern::{MatchPattern, MatchPatternSlice, MatchType};

use crate::tools::acl;
use crate::tools::fs;
use crate::tools::xattr;

// This one need Read, but works without Seek
pub struct SequentialDecoder<R: Read> {
    reader: R,
    feature_flags: u64,
    allow_existing_dirs: bool,
    skip_buffer: Vec<u8>,
    callback: Option<Box<dyn Fn(&Path) -> Result<(), Error> + Send>>,
}

const HEADER_SIZE: u64 = std::mem::size_of::<PxarHeader>() as u64;

impl<R: Read> SequentialDecoder<R> {

    pub fn new(
        reader: R,
        feature_flags: u64,
    ) -> Self {
        let skip_buffer = vec::undefined(64 * 1024);

        Self {
            reader,
            feature_flags,
            allow_existing_dirs: false,
            skip_buffer,
            callback: None,
        }
    }

    pub fn set_callback<F: Fn(&Path) -> Result<(), Error> + Send + 'static>(&mut self, callback: F ) {
        self.callback = Some(Box::new(callback));
    }

    pub fn set_allow_existing_dirs(&mut self, allow: bool) {
        self.allow_existing_dirs = allow;
    }

    pub(crate) fn get_reader_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    pub(crate) fn read_item<T: Endian>(&mut self) -> Result<T, Error> {
        let mut result = std::mem::MaybeUninit::<T>::uninit();

        let buffer = unsafe {
            std::slice::from_raw_parts_mut(result.as_mut_ptr() as *mut u8, std::mem::size_of::<T>())
        };

        self.reader.read_exact(buffer)?;
        let result = unsafe { result.assume_init() };

        Ok(result.from_le())
    }

    pub(crate) fn read_link(&mut self, size: u64) -> Result<PathBuf, Error> {
        if size < (HEADER_SIZE + 2) {
            bail!("dectected short link target.");
        }
        let target_len = size - HEADER_SIZE;

        if target_len > (libc::PATH_MAX as u64) {
            bail!("link target too long ({}).", target_len);
        }

        let mut buffer = self.reader.read_exact_allocated(target_len as usize)?;

        let last_byte = buffer.pop().unwrap();
        if last_byte != 0u8 {
            bail!("link target not nul terminated.");
        }

        Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
    }

    pub(crate) fn read_hardlink(&mut self, size: u64) -> Result<(PathBuf, u64), Error> {
        if size < (HEADER_SIZE + 8 + 2) {
            bail!("dectected short hardlink header.");
        }
        let offset: u64 = self.read_item()?;
        let target = self.read_link(size - 8)?;

        for c in target.components() {
            match c {
                std::path::Component::Normal(_) => { /* OK */ }
                _ => bail!("hardlink target contains invalid component {:?}", c),
            }
        }

        Ok((target, offset))
    }

    pub(crate) fn read_filename(&mut self, size: u64) -> Result<OsString, Error> {
        if size < (HEADER_SIZE + 2) {
            bail!("dectected short filename");
        }
        let name_len = size - HEADER_SIZE;

        if name_len > ((libc::FILENAME_MAX as u64) + 1) {
            bail!("filename too long ({}).", name_len);
        }

        let mut buffer = self.reader.read_exact_allocated(name_len as usize)?;

        let last_byte = buffer.pop().unwrap();
        if last_byte != 0u8 {
            bail!("filename entry not nul terminated.");
        }

        if buffer == b"." || buffer == b".." {
            bail!("found invalid filename '.' or '..'.");
        }

        if buffer.iter().any(|b| (*b == b'/' || *b == b'\0')) {
            bail!("found invalid filename with slashes or nul bytes.");
        }

        let name = std::ffi::OsString::from_vec(buffer);
        if name.is_empty() {
            bail!("found empty filename.");
        }

        Ok(name)
    }

    fn has_features(&self, feature_flags: u64) -> bool {
        (self.feature_flags & feature_flags) == feature_flags
    }

    fn read_xattr(&mut self, size: usize) -> Result<PxarXAttr, Error> {
        let buffer = self.reader.read_exact_allocated(size)?;

        let separator = buffer
            .iter()
            .position(|c| *c == b'\0')
            .ok_or_else(|| format_err!("no value found in xattr"))?;

        let (name, value) = buffer.split_at(separator);
        if !xattr::is_valid_xattr_name(name) || xattr::is_security_capability(name) {
            bail!("incorrect xattr name - {}.", String::from_utf8_lossy(name));
        }

        Ok(PxarXAttr {
            name: name.to_vec(),
            value: value[1..].to_vec(),
        })
    }

    fn read_fcaps(&mut self, size: usize) -> Result<PxarFCaps, Error> {
        let buffer = self.reader.read_exact_allocated(size)?;

        Ok(PxarFCaps { data: buffer })
    }

    pub(crate) fn read_attributes(&mut self) -> Result<(PxarHeader, PxarAttributes), Error> {
        let mut attr = PxarAttributes::default();
        let mut head: PxarHeader = self.read_item()?;
        let mut size = (head.size - HEADER_SIZE) as usize;
        loop {
            match head.htype {
                PXAR_XATTR => {
                    if self.has_features(flags::WITH_XATTRS) {
                        attr.xattrs.push(self.read_xattr(size)?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_FCAPS => {
                    if self.has_features(flags::WITH_FCAPS) {
                        attr.fcaps = Some(self.read_fcaps(size)?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_ACL_USER => {
                    if self.has_features(flags::WITH_ACL) {
                        attr.acl_user.push(self.read_item::<PxarACLUser>()?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_ACL_GROUP => {
                    if self.has_features(flags::WITH_ACL) {
                        attr.acl_group.push(self.read_item::<PxarACLGroup>()?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_ACL_GROUP_OBJ => {
                    if self.has_features(flags::WITH_ACL) {
                        attr.acl_group_obj = Some(self.read_item::<PxarACLGroupObj>()?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_ACL_DEFAULT => {
                    if self.has_features(flags::WITH_ACL) {
                        attr.acl_default = Some(self.read_item::<PxarACLDefault>()?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_ACL_DEFAULT_USER => {
                    if self.has_features(flags::WITH_ACL) {
                        attr.acl_default_user.push(self.read_item::<PxarACLUser>()?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_ACL_DEFAULT_GROUP => {
                    if self.has_features(flags::WITH_ACL) {
                        attr.acl_default_group
                            .push(self.read_item::<PxarACLGroup>()?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                PXAR_QUOTA_PROJID => {
                    if self.has_features(flags::WITH_QUOTA_PROJID) {
                        attr.quota_projid = Some(self.read_item::<PxarQuotaProjID>()?);
                    } else {
                        self.skip_bytes(size)?;
                    }
                }
                _ => break,
            }
            head = self.read_item()?;
            size = (head.size - HEADER_SIZE) as usize;
        }

        Ok((head, attr))
    }

    fn restore_attributes(
        &mut self,
        fd: RawFd,
        attr: &PxarAttributes,
        entry: &PxarEntry,
    ) -> Result<(), Error> {
        self.restore_xattrs_fcaps_fd(fd, &attr.xattrs, &attr.fcaps)?;

        let mut acl = acl::ACL::init(5)?;
        acl.add_entry_full(
            acl::ACL_USER_OBJ,
            None,
            mode_user_to_acl_permissions(entry.mode),
        )?;
        acl.add_entry_full(
            acl::ACL_OTHER,
            None,
            mode_other_to_acl_permissions(entry.mode),
        )?;
        match &attr.acl_group_obj {
            Some(group_obj) => {
                acl.add_entry_full(
                    acl::ACL_MASK,
                    None,
                    mode_group_to_acl_permissions(entry.mode),
                )?;
                acl.add_entry_full(acl::ACL_GROUP_OBJ, None, group_obj.permissions)?;
            }
            None => {
                acl.add_entry_full(
                    acl::ACL_GROUP_OBJ,
                    None,
                    mode_group_to_acl_permissions(entry.mode),
                )?;
            }
        }
        for user in &attr.acl_user {
            acl.add_entry_full(acl::ACL_USER, Some(user.uid), user.permissions)?;
        }
        for group in &attr.acl_group {
            acl.add_entry_full(acl::ACL_GROUP, Some(group.gid), group.permissions)?;
        }
        let proc_path = Path::new("/proc/self/fd/").join(fd.to_string());
        if !acl.is_valid() {
            bail!("Error while restoring ACL - ACL invalid");
        }
        acl.set_file(&proc_path, acl::ACL_TYPE_ACCESS)?;

        if let Some(default) = &attr.acl_default {
            let mut acl = acl::ACL::init(5)?;
            acl.add_entry_full(acl::ACL_USER_OBJ, None, default.user_obj_permissions)?;
            acl.add_entry_full(acl::ACL_GROUP_OBJ, None, default.group_obj_permissions)?;
            acl.add_entry_full(acl::ACL_OTHER, None, default.other_permissions)?;
            if default.mask_permissions != std::u64::MAX {
                acl.add_entry_full(acl::ACL_MASK, None, default.mask_permissions)?;
            }
            for user in &attr.acl_default_user {
                acl.add_entry_full(acl::ACL_USER, Some(user.uid), user.permissions)?;
            }
            for group in &attr.acl_default_group {
                acl.add_entry_full(acl::ACL_GROUP, Some(group.gid), group.permissions)?;
            }
            if !acl.is_valid() {
                bail!("Error while restoring ACL - ACL invalid");
            }
            acl.set_file(&proc_path, acl::ACL_TYPE_DEFAULT)?;
        }
        self.restore_quota_projid(fd, &attr.quota_projid)?;

        Ok(())
    }

    // Restore xattrs and fcaps to the given RawFd.
    fn restore_xattrs_fcaps_fd(
        &mut self,
        fd: RawFd,
        xattrs: &[PxarXAttr],
        fcaps: &Option<PxarFCaps>,
    ) -> Result<(), Error> {
        for xattr in xattrs {
            if let Err(err) = xattr::fsetxattr(fd, &xattr) {
                bail!("fsetxattr failed with error: {}", err);
            }
        }
        if let Some(fcaps) = fcaps {
            if let Err(err) = xattr::fsetxattr_fcaps(fd, &fcaps) {
                bail!("fsetxattr_fcaps failed with error: {}", err);
            }
        }

        Ok(())
    }

    fn restore_quota_projid(
        &mut self,
        fd: RawFd,
        projid: &Option<PxarQuotaProjID>,
    ) -> Result<(), Error> {
        if let Some(projid) = projid {
            let mut fsxattr = fs::FSXAttr::default();
            unsafe {
                fs::fs_ioc_fsgetxattr(fd, &mut fsxattr).map_err(|err| {
                    format_err!(
                        "error while getting fsxattr to restore quota project id - {}",
                        err
                    )
                })?;
            }
            fsxattr.fsx_projid = projid.projid as u32;
            unsafe {
                fs::fs_ioc_fssetxattr(fd, &fsxattr).map_err(|err| {
                    format_err!(
                        "error while setting fsxattr to restore quota project id - {}",
                        err
                    )
                })?;
            }
        }

        Ok(())
    }

    fn restore_mode(&mut self, entry: &PxarEntry, fd: RawFd) -> Result<(), Error> {
        let mode = Mode::from_bits_truncate((entry.mode as u32) & 0o7777);

        nix::sys::stat::fchmod(fd, mode)?;

        Ok(())
    }

    fn restore_mode_at(
        &mut self,
        entry: &PxarEntry,
        dirfd: RawFd,
        filename: &OsStr,
    ) -> Result<(), Error> {
        let mode = Mode::from_bits_truncate((entry.mode as u32) & 0o7777);

        // NOTE: we want :FchmodatFlags::NoFollowSymlink, but fchmodat does not support that
        // on linux (see man fchmodat). Fortunately, we can simply avoid calling this on symlinks.
        nix::sys::stat::fchmodat(
            Some(dirfd),
            filename,
            mode,
            nix::sys::stat::FchmodatFlags::FollowSymlink,
        )?;

        Ok(())
    }

    fn restore_ugid(&mut self, entry: &PxarEntry, fd: RawFd) -> Result<(), Error> {
        let uid = entry.uid;
        let gid = entry.gid;

        let res = unsafe { libc::fchown(fd, uid, gid) };
        Errno::result(res)?;

        Ok(())
    }

    fn restore_ugid_at(
        &mut self,
        entry: &PxarEntry,
        dirfd: RawFd,
        filename: &OsStr,
    ) -> Result<(), Error> {
        let uid = entry.uid;
        let gid = entry.gid;

        let res = filename.with_nix_path(|cstr| unsafe {
            libc::fchownat(dirfd, cstr.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW)
        })?;
        Errno::result(res)?;

        Ok(())
    }

    fn restore_mtime(&mut self, entry: &PxarEntry, fd: RawFd) -> Result<(), Error> {
        let times = nsec_to_update_timespec(entry.mtime);

        let res = unsafe { libc::futimens(fd, &times[0]) };
        Errno::result(res)?;

        Ok(())
    }

    fn restore_mtime_at(
        &mut self,
        entry: &PxarEntry,
        dirfd: RawFd,
        filename: &OsStr,
    ) -> Result<(), Error> {
        let times = nsec_to_update_timespec(entry.mtime);

        let res = filename.with_nix_path(|cstr| unsafe {
            libc::utimensat(dirfd, cstr.as_ptr(), &times[0], libc::AT_SYMLINK_NOFOLLOW)
        })?;
        Errno::result(res)?;

        Ok(())
    }

    fn restore_device_at(
        &mut self,
        entry: &PxarEntry,
        dirfd: RawFd,
        filename: &OsStr,
        device: &PxarDevice,
    ) -> Result<(), Error> {
        let rdev = nix::sys::stat::makedev(device.major, device.minor);
        let mode = ((entry.mode as u32) & libc::S_IFMT) | 0o0600;
        let res = filename
            .with_nix_path(|cstr| unsafe { libc::mknodat(dirfd, cstr.as_ptr(), mode, rdev) })?;
        Errno::result(res)?;

        Ok(())
    }

    fn restore_socket_at(&mut self, dirfd: RawFd, filename: &OsStr) -> Result<(), Error> {
        let mode = libc::S_IFSOCK | 0o0600;
        let res = filename
            .with_nix_path(|cstr| unsafe { libc::mknodat(dirfd, cstr.as_ptr(), mode, 0) })?;
        Errno::result(res)?;

        Ok(())
    }

    fn restore_fifo_at(&mut self, dirfd: RawFd, filename: &OsStr) -> Result<(), Error> {
        let mode = libc::S_IFIFO | 0o0600;
        let res =
            filename.with_nix_path(|cstr| unsafe { libc::mkfifoat(dirfd, cstr.as_ptr(), mode) })?;
        Errno::result(res)?;

        Ok(())
    }

    pub(crate) fn skip_bytes(&mut self, count: usize) -> Result<(), Error> {
        let mut done = 0;
        while done < count {
            let todo = count - done;
            let n = if todo > self.skip_buffer.len() {
                self.skip_buffer.len()
            } else {
                todo
            };
            let data = &mut self.skip_buffer[..n];
            self.reader.read_exact(data)?;
            done += n;
        }
        Ok(())
    }

    fn restore_symlink(
        &mut self,
        parent_fd: Option<RawFd>,
        full_path: &PathBuf,
        entry: &PxarEntry,
        filename: &OsStr,
    ) -> Result<(), Error> {
        //fixme: create symlink
        //fixme: restore permission, acls, xattr, ...

        let head: PxarHeader = self.read_item()?;
        match head.htype {
            PXAR_SYMLINK => {
                let target = self.read_link(head.size)?;
                //println!("TARGET: {:?}", target);
                if let Some(fd) = parent_fd {
                    if let Err(err) = symlinkat(&target, fd, filename) {
                        bail!("create symlink {:?} failed - {}", full_path, err);
                    }
                }
            }
            _ => bail!(
                "got unknown header type inside symlink entry {:016x}",
                head.htype
            ),
        }

        if let Some(fd) = parent_fd {
            // self.restore_mode_at(&entry, fd, filename)?; //not supported on symlinks
            self.restore_ugid_at(&entry, fd, filename)?;
            self.restore_mtime_at(&entry, fd, filename)?;
        }

        Ok(())
    }

    fn restore_socket(
        &mut self,
        parent_fd: Option<RawFd>,
        entry: &PxarEntry,
        filename: &OsStr,
    ) -> Result<(), Error> {
        if !self.has_features(flags::WITH_SOCKETS) {
            return Ok(());
        }
        if let Some(fd) = parent_fd {
            self.restore_socket_at(fd, filename)?;
            self.restore_mode_at(&entry, fd, filename)?;
            self.restore_ugid_at(&entry, fd, filename)?;
            self.restore_mtime_at(&entry, fd, filename)?;
        }

        Ok(())
    }

    fn restore_fifo(
        &mut self,
        parent_fd: Option<RawFd>,
        entry: &PxarEntry,
        filename: &OsStr,
    ) -> Result<(), Error> {
        if !self.has_features(flags::WITH_FIFOS) {
            return Ok(());
        }
        if let Some(fd) = parent_fd {
            self.restore_fifo_at(fd, filename)?;
            self.restore_mode_at(&entry, fd, filename)?;
            self.restore_ugid_at(&entry, fd, filename)?;
            self.restore_mtime_at(&entry, fd, filename)?;
        }

        Ok(())
    }

    fn restore_device(
        &mut self,
        parent_fd: Option<RawFd>,
        entry: &PxarEntry,
        filename: &OsStr,
    ) -> Result<(), Error> {
        let head: PxarHeader = self.read_item()?;
        if head.htype != PXAR_DEVICE {
            bail!(
                "got unknown header type inside device entry {:016x}",
                head.htype
            );
        }
        let device: PxarDevice = self.read_item()?;
        if !self.has_features(flags::WITH_DEVICE_NODES) {
            return Ok(());
        }
        if let Some(fd) = parent_fd {
            self.restore_device_at(&entry, fd, filename, &device)?;
            self.restore_mode_at(&entry, fd, filename)?;
            self.restore_ugid_at(&entry, fd, filename)?;
            self.restore_mtime_at(&entry, fd, filename)?;
        }

        Ok(())
    }

    /// Restores a regular file with its content and associated attributes to the
    /// folder provided by the raw filedescriptor.
    /// If None is passed instead of a filedescriptor, the file is not restored but
    /// the archive reader is skipping over it instead.
    fn restore_regular_file(
        &mut self,
        parent_fd: Option<RawFd>,
        full_path: &PathBuf,
        entry: &PxarEntry,
        filename: &OsStr,
    ) -> Result<(), Error> {
        let (head, attr) = self
            .read_attributes()
            .map_err(|err| format_err!("Reading of file attributes failed - {}", err))?;

        if let Some(fd) = parent_fd {
            let flags = OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_EXCL;
            let open_mode = Mode::from_bits_truncate(0o0600 | entry.mode as u32); //fixme: upper 32bits of entry.mode?
            let mut file = file_openat(fd, filename, flags, open_mode)
                .map_err(|err| format_err!("open file {:?} failed - {}", full_path, err))?;

            if head.htype != PXAR_PAYLOAD {
                bail!("got unknown header type for file entry {:016x}", head.htype);
            }

            if head.size < HEADER_SIZE {
                bail!("detected short payload");
            }
            let need = (head.size - HEADER_SIZE) as usize;

            let mut read_buffer = unsafe { vec::uninitialized(64 * 1024) };
            let mut done = 0;
            while done < need {
                let todo = need - done;
                let n = if todo > read_buffer.len() {
                    read_buffer.len()
                } else {
                    todo
                };
                let data = &mut read_buffer[..n];
                self.reader.read_exact(data)?;
                file.write_all(data)?;
                done += n;
            }

            self.restore_ugid(&entry, file.as_raw_fd())?;
            // fcaps have to be restored after restore_ugid as chown clears security.capability xattr, see CVE-2015-1350
            self.restore_attributes(file.as_raw_fd(), &attr, &entry)?;
            self.restore_mode(&entry, file.as_raw_fd())?;
            self.restore_mtime(&entry, file.as_raw_fd())?;
        } else {
            if head.htype != PXAR_PAYLOAD {
                bail!("got unknown header type for file entry {:016x}", head.htype);
            }
            if head.size < HEADER_SIZE {
                bail!("detected short payload");
            }
            self.skip_bytes((head.size - HEADER_SIZE) as usize)?;
        }

        Ok(())
    }

    fn restore_dir(
        &mut self,
        base_path: &Path,
        dirs: &mut PxarDirStack,
        entry: PxarEntry,
        filename: &OsStr,
        matched: MatchType,
        match_pattern: &[MatchPatternSlice],
    ) -> Result<(), Error> {
        let (mut head, attr) = self
            .read_attributes()
            .map_err(|err| format_err!("Reading of directory attributes failed - {}", err))?;

        let dir = PxarDir::new(filename, entry, attr);
        dirs.push(dir);
        if matched == MatchType::Positive {
            dirs.create_all_dirs(!self.allow_existing_dirs)?;
        }

        while head.htype == PXAR_FILENAME {
            let name = self.read_filename(head.size)?;
            self.restore_dir_entry(base_path, dirs, &name, matched, match_pattern)?;
            head = self.read_item()?;
        }

        if head.htype != PXAR_GOODBYE {
            bail!(
                "got unknown header type inside directory entry {:016x}",
                head.htype
            );
        }

        if head.size < HEADER_SIZE {
            bail!("detected short goodbye table");
        }
        self.skip_bytes((head.size - HEADER_SIZE) as usize)?;

        let last = dirs
            .pop()
            .ok_or_else(|| format_err!("Tried to pop beyond dir root - this should not happen!"))?;
        if let Some(d) = last.dir {
            let fd = d.as_raw_fd();
            self.restore_ugid(&last.entry, fd)?;
            // fcaps have to be restored after restore_ugid as chown clears security.capability xattr, see CVE-2015-1350
            self.restore_attributes(fd, &last.attr, &last.entry)?;
            self.restore_mode(&last.entry, fd)?;
            self.restore_mtime(&last.entry, fd)?;
        }

        Ok(())
    }

    /// Restore an archive into the specified directory.
    ///
    /// The directory is created if it does not exist.
    pub fn restore(&mut self, path: &Path, match_pattern: &[MatchPattern]) -> Result<(), Error> {
        let mut slices = Vec::new();
        for pattern in match_pattern {
            slices.push(pattern.as_slice());
        }
        std::fs::create_dir_all(path)
            .map_err(|err| format_err!("error while creating directory {:?} - {}", path, err))?;

        let dir = nix::dir::Dir::open(
            path,
            nix::fcntl::OFlag::O_DIRECTORY,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|err| format_err!("unable to open target directory {:?} - {}", path, err))?;
        let fd = dir.as_raw_fd();
        let mut dirs = PxarDirStack::new(fd);
        // An empty match pattern list indicates to restore the full archive.
        let matched = if slices.is_empty() {
            MatchType::Positive
        } else {
            MatchType::None
        };

        let header: PxarHeader = self.read_item()?;
        check_ca_header::<PxarEntry>(&header, PXAR_ENTRY)?;
        let entry: PxarEntry = self.read_item()?;

        let (mut head, attr) = self
            .read_attributes()
            .map_err(|err| format_err!("Reading of directory attributes failed - {}", err))?;

        while head.htype == PXAR_FILENAME {
            let name = self.read_filename(head.size)?;
            self.restore_dir_entry(path, &mut dirs, &name, matched, &slices)?;
            head = self.read_item()?;
        }

        if head.htype != PXAR_GOODBYE {
            bail!(
                "got unknown header type inside directory entry {:016x}",
                head.htype
            );
        }

        if head.size < HEADER_SIZE {
            bail!("detected short goodbye table");
        }
        self.skip_bytes((head.size - HEADER_SIZE) as usize)?;

        self.restore_ugid(&entry, fd)?;
        // fcaps have to be restored after restore_ugid as chown clears security.capability xattr, see CVE-2015-1350
        self.restore_attributes(fd, &attr, &entry)?;
        self.restore_mode(&entry, fd)?;
        self.restore_mtime(&entry, fd)?;

        Ok(())
    }

    fn restore_dir_entry(
        &mut self,
        base_path: &Path,
        dirs: &mut PxarDirStack,
        filename: &OsStr,
        parent_matched: MatchType,
        match_pattern: &[MatchPatternSlice],
    ) -> Result<(), Error> {
        let relative_path = dirs.as_path_buf();
        let full_path = base_path.join(&relative_path).join(filename);

        let head: PxarHeader = self.read_item()?;
        if head.htype == PXAR_FORMAT_HARDLINK {
            let (target, _offset) = self.read_hardlink(head.size)?;
            let target_path = base_path.join(&target);
            if dirs.last_dir_fd().is_some() {
                if let Some(ref callback) = self.callback {
                    (callback)(&full_path)?;
                }
                hardlink(&target_path, &full_path)?;
            }
            return Ok(());
        }

        check_ca_header::<PxarEntry>(&head, PXAR_ENTRY)?;
        let entry: PxarEntry = self.read_item()?;
        let ifmt = entry.mode as u32 & libc::S_IFMT;

        let mut child_pattern = Vec::new();
        // If parent was a match, then children should be assumed to match too
        // This is especially the case when the full archive is restored and
        // there are no match pattern.
        let mut matched = parent_matched;
        if !match_pattern.is_empty() {
            match MatchPatternSlice::match_filename_include(
                &CString::new(filename.as_bytes())?,
                ifmt == libc::S_IFDIR,
                match_pattern,
            )? {
                (MatchType::None, _) => matched = MatchType::None,
                (MatchType::Negative, _) => matched = MatchType::Negative,
                (MatchType::Positive, _) => matched = MatchType::Positive,
                (match_type, pattern) => {
                    matched = match_type;
                    child_pattern = pattern;
                }
            }
        }

        let fd = if matched == MatchType::Positive {
            Some(dirs.create_all_dirs(!self.allow_existing_dirs)?)
        } else {
            None
        };

        if fd.is_some() {
            if let Some(ref callback) = self.callback {
                (callback)(&full_path)?;
            }
        }

        match ifmt {
            libc::S_IFDIR => {
                self.restore_dir(base_path, dirs, entry, &filename, matched, &child_pattern)
            }
            libc::S_IFLNK => self.restore_symlink(fd, &full_path, &entry, &filename),
            libc::S_IFSOCK => self.restore_socket(fd, &entry, &filename),
            libc::S_IFIFO => self.restore_fifo(fd, &entry, &filename),
            libc::S_IFBLK | libc::S_IFCHR => self.restore_device(fd, &entry, &filename),
            libc::S_IFREG => self.restore_regular_file(fd, &full_path, &entry, &filename),
            _ => Ok(()),
        }
    }

    /// List/Dump archive content.
    ///
    /// Simply print the list of contained files. This dumps archive
    /// format details when the verbose flag is set (useful for debug).
    pub fn dump_entry<W: std::io::Write>(
        &mut self,
        path: &mut PathBuf,
        verbose: bool,
        output: &mut W,
    ) -> Result<(), Error> {
        let print_head = |head: &PxarHeader| {
            println!("Type: {:016x}", head.htype);
            println!("Size: {}", head.size);
        };

        let head: PxarHeader = self.read_item()?;
        if verbose {
            println!("Path: {:?}", path);
            print_head(&head);
        } else {
            println!("{:?}", path);
        }

        if head.htype == PXAR_FORMAT_HARDLINK {
            let (target, offset) = self.read_hardlink(head.size)?;
            if verbose {
                println!("Hardlink: {} {:?}", offset, target);
            }
            return Ok(());
        }

        check_ca_header::<PxarEntry>(&head, PXAR_ENTRY)?;
        let entry: PxarEntry = self.read_item()?;

        if verbose {
            println!(
                "Mode: {:08x} {:08x}",
                entry.mode,
                (entry.mode as u32) & libc::S_IFDIR
            );
        }

        let ifmt = (entry.mode as u32) & libc::S_IFMT;

        if ifmt == libc::S_IFDIR {
            let mut entry_count = 0;

            loop {
                let head: PxarHeader = self.read_item()?;
                if verbose {
                    print_head(&head);
                }

                // This call covers all the cases of the match statement
                // regarding extended attributes. These calls will never
                // break on the loop and can therefore be handled separately.
                // If the header was matched, true is returned and we can continue
                if self.dump_if_attribute(&head, verbose)? {
                    continue;
                }

                match head.htype {
                    PXAR_FILENAME => {
                        let name = self.read_filename(head.size)?;
                        if verbose {
                            println!("Name: {:?}", name);
                        }
                        entry_count += 1;
                        path.push(&name);
                        self.dump_entry(path, verbose, output)?;
                        path.pop();
                    }
                    PXAR_GOODBYE => {
                        let table_size = (head.size - HEADER_SIZE) as usize;
                        if verbose {
                            println!("Goodbye: {:?}", path);
                            self.dump_goodby_entries(entry_count, table_size)?;
                        } else {
                            self.skip_bytes(table_size)?;
                        }
                        break;
                    }
                    _ => panic!("got unexpected header type inside directory"),
                }
            }
        } else if (ifmt == libc::S_IFBLK)
            || (ifmt == libc::S_IFCHR)
            || (ifmt == libc::S_IFLNK)
            || (ifmt == libc::S_IFREG)
        {
            loop {
                let head: PxarHeader = self.read_item()?;
                if verbose {
                    print_head(&head);
                }

                // This call covers all the cases of the match statement
                // regarding extended attributes. These calls will never
                // break on the loop and can therefore be handled separately.
                // If the header was matched, true is returned and we can continue
                if self.dump_if_attribute(&head, verbose)? {
                    continue;
                }

                match head.htype {
                    PXAR_SYMLINK => {
                        let target = self.read_link(head.size)?;
                        if verbose {
                            println!("Symlink: {:?}", target);
                        }
                        break;
                    }
                    PXAR_DEVICE => {
                        let device: PxarDevice = self.read_item()?;
                        if verbose {
                            println!("Device: {}, {}", device.major, device.minor);
                        }
                        break;
                    }
                    PXAR_PAYLOAD => {
                        let payload_size = (head.size - HEADER_SIZE) as usize;
                        if verbose {
                            println!("Payload: {}", payload_size);
                        }
                        self.skip_bytes(payload_size)?;
                        break;
                    }
                    _ => {
                        panic!("got unexpected header type inside non-directory");
                    }
                }
            }
        } else if ifmt == libc::S_IFIFO {
            if verbose {
                println!("Fifo:");
            }
        } else if ifmt == libc::S_IFSOCK {
            if verbose {
                println!("Socket:");
            }
        } else {
            panic!("unknown st_mode");
        }
        Ok(())
    }

    fn dump_if_attribute(&mut self, header: &PxarHeader, verbose: bool) -> Result<bool, Error> {
        match header.htype {
            PXAR_XATTR => {
                let xattr = self.read_xattr((header.size - HEADER_SIZE) as usize)?;
                if verbose && self.has_features(flags::WITH_XATTRS) {
                    println!("XAttr: {:?}", xattr);
                }
            }
            PXAR_FCAPS => {
                let fcaps = self.read_fcaps((header.size - HEADER_SIZE) as usize)?;
                if verbose && self.has_features(flags::WITH_FCAPS) {
                    println!("FCaps: {:?}", fcaps);
                }
            }
            PXAR_ACL_USER => {
                let user = self.read_item::<PxarACLUser>()?;
                if verbose && self.has_features(flags::WITH_ACL) {
                    println!("ACLUser: {:?}", user);
                }
            }
            PXAR_ACL_GROUP => {
                let group = self.read_item::<PxarACLGroup>()?;
                if verbose && self.has_features(flags::WITH_ACL) {
                    println!("ACLGroup: {:?}", group);
                }
            }
            PXAR_ACL_GROUP_OBJ => {
                let group_obj = self.read_item::<PxarACLGroupObj>()?;
                if verbose && self.has_features(flags::WITH_ACL) {
                    println!("ACLGroupObj: {:?}", group_obj);
                }
            }
            PXAR_ACL_DEFAULT => {
                let default = self.read_item::<PxarACLDefault>()?;
                if verbose && self.has_features(flags::WITH_ACL) {
                    println!("ACLDefault: {:?}", default);
                }
            }
            PXAR_ACL_DEFAULT_USER => {
                let default_user = self.read_item::<PxarACLUser>()?;
                if verbose && self.has_features(flags::WITH_ACL) {
                    println!("ACLDefaultUser: {:?}", default_user);
                }
            }
            PXAR_ACL_DEFAULT_GROUP => {
                let default_group = self.read_item::<PxarACLGroup>()?;
                if verbose && self.has_features(flags::WITH_ACL) {
                    println!("ACLDefaultGroup: {:?}", default_group);
                }
            }
            PXAR_QUOTA_PROJID => {
                let quota_projid = self.read_item::<PxarQuotaProjID>()?;
                if verbose && self.has_features(flags::WITH_QUOTA_PROJID) {
                    println!("Quota project id: {:?}", quota_projid);
                }
            }
            _ => return Ok(false),
        }

        Ok(true)
    }

    fn dump_goodby_entries(&mut self, entry_count: usize, table_size: usize) -> Result<(), Error> {
        const GOODBYE_ITEM_SIZE: usize = std::mem::size_of::<PxarGoodbyeItem>();

        if table_size < GOODBYE_ITEM_SIZE {
            bail!(
                "Goodbye table to small ({} < {})",
                table_size,
                GOODBYE_ITEM_SIZE
            );
        }
        if (table_size % GOODBYE_ITEM_SIZE) != 0 {
            bail!("Goodbye table with strange size ({})", table_size);
        }

        let entries = table_size / GOODBYE_ITEM_SIZE;

        if entry_count != (entries - 1) {
            bail!(
                "Goodbye table with wrong entry count ({} != {})",
                entry_count,
                entries - 1
            );
        }

        let mut count = 0;

        loop {
            let item: PxarGoodbyeItem = self.read_item()?;
            count += 1;
            if item.hash == PXAR_GOODBYE_TAIL_MARKER {
                if count != entries {
                    bail!("unexpected goodbye tail marker");
                }
                println!("Goodby tail mark.");
                break;
            }
            println!(
                "Goodby item: offset {}, size {}, hash {:016x}",
                item.offset, item.size, item.hash
            );
            if count >= entries {
                bail!("too many goodbye items (no tail marker)");
            }
        }

        Ok(())
    }
}

fn file_openat(
    parent: RawFd,
    filename: &OsStr,
    flags: OFlag,
    mode: Mode,
) -> Result<std::fs::File, Error> {
    let fd =
        filename.with_nix_path(|cstr| nix::fcntl::openat(parent, cstr, flags, mode))??;

    let file = unsafe { std::fs::File::from_raw_fd(fd) };

    Ok(file)
}

fn hardlink(oldpath: &Path, newpath: &Path) -> Result<(), Error> {
    oldpath.with_nix_path(|oldpath| {
        newpath.with_nix_path(|newpath| {
            let res = unsafe { libc::link(oldpath.as_ptr(), newpath.as_ptr()) };
            Errno::result(res)?;
            Ok(())
        })?
    })?
}

fn symlinkat(target: &Path, parent: RawFd, linkname: &OsStr) -> Result<(), Error> {
    target.with_nix_path(|target| {
        linkname.with_nix_path(|linkname| {
            let res = unsafe { libc::symlinkat(target.as_ptr(), parent, linkname.as_ptr()) };
            Errno::result(res)?;
            Ok(())
        })?
    })?
}

fn nsec_to_update_timespec(mtime_nsec: u64) -> [libc::timespec; 2] {
    // restore mtime
    const UTIME_OMIT: i64 = ((1 << 30) - 2);
    const NANOS_PER_SEC: i64 = 1_000_000_000;

    let sec = (mtime_nsec as i64) / NANOS_PER_SEC;
    let nsec = (mtime_nsec as i64) % NANOS_PER_SEC;

    let times: [libc::timespec; 2] = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        },
    ];

    times
}

fn mode_user_to_acl_permissions(mode: u64) -> u64 {
    (mode >> 6) & 7
}

fn mode_group_to_acl_permissions(mode: u64) -> u64 {
    (mode >> 3) & 7
}

fn mode_other_to_acl_permissions(mode: u64) -> u64 {
    mode & 7
}
