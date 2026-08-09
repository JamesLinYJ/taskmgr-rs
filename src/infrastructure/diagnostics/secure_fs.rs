// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 句柄锚定的诊断文件系统边界
//
//   文件:       src/infrastructure/diagnostics/secure_fs.rs
//
//   日期:       2026年07月31日
//   环境:       Windows NT 10.0.29634 x86_64；Rust 1.97.0 (MSVC)
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 诊断日志和导出包的 Windows 文件系统安全边界。
//!
//! 所有不可信路径只用于打开第一个卷/共享根。后续组件都通过已验证的父目录句柄
//! 相对打开，并使用 `FILE_OPEN_REPARSE_POINT`。这样目录改名、junction/symlink
//! 替换和文件名竞态都不能把高完整性进程的读写重定向到锚定目录之外。

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::{copy_nonoverlapping, null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DIRECTORY_INFORMATION, FILE_NON_DIRECTORY_FILE,
    FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
    FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile, NtQueryDirectoryFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, INVALID_HANDLE_VALUE, LocalFree, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
    STATUS_BUFFER_OVERFLOW, STATUS_NO_MORE_FILES, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, DELETE, FILE_ATTRIBUTE_DEVICE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileAttributeTagInfo,
    FileDispositionInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_EXISTING,
    SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::infrastructure::native::OwnedHandle;

const DIRECTORY_QUERY_BUFFER_BYTES: usize = 64 * 1024;
const TRAVERSE_ACCESS: u32 = FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const DIRECTORY_ACCESS: u32 = TRAVERSE_ACCESS | FILE_LIST_DIRECTORY;
const HIGH_INTEGRITY_DIRECTORY_LABEL: &str = "S:(ML;OICI;NW;;;HI)";
const HIGH_INTEGRITY_FILE_LABEL: &str = "S:(ML;;NW;;;HI)";
const MEDIUM_INTEGRITY_FILE_LABEL: &str = "S:(ML;;NW;;;ME)";

pub(super) struct SecureDirectory {
    file: File,
    path: PathBuf,
}

#[derive(Debug)]
pub(super) struct SecureDirectoryEntry {
    pub(super) name: OsString,
    pub(super) attributes: u32,
    pub(super) last_write_time: i64,
}

impl SecureDirectory {
    pub(super) fn open_absolute(path: &Path) -> io::Result<Self> {
        Self::walk_absolute(path, false)
    }

    pub(super) fn open_or_create_absolute(path: &Path) -> io::Result<Self> {
        Self::walk_absolute(path, true)
    }

    fn walk_absolute(path: &Path, create_missing: bool) -> io::Result<Self> {
        let (root_path, components) = split_absolute_path(path)?;
        let mut directory =
            open_volume_or_share_root(&root_path, components.is_empty()).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("unable to open path root {}: {error}", root_path.display()),
                )
            })?;
        let component_count = components.len();
        for (index, component) in components.into_iter().enumerate() {
            let enumerate = index + 1 == component_count;
            directory = if create_missing {
                directory
                    .open_directory_with(&component, FILE_OPEN_IF, false, false, enumerate)
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!(
                                "unable to securely open or create {}: {error}",
                                directory.path.join(&component).display()
                            ),
                        )
                    })?
            } else {
                directory
                    .open_directory_with(&component, FILE_OPEN, false, false, enumerate)
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!(
                                "unable to securely open {}: {error}",
                                directory.path.join(&component).display()
                            ),
                        )
                    })?
            };
        }
        directory.path = path.to_path_buf();
        Ok(directory)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn raw_handle(&self) -> HANDLE {
        self.file.as_raw_handle() as HANDLE
    }

    pub(super) fn open_directory(&self, name: &OsStr) -> io::Result<Self> {
        self.open_directory_with(name, FILE_OPEN, false, false, true)
    }

    pub(super) fn open_directory_for_delete(&self, name: &OsStr) -> io::Result<Self> {
        self.open_directory_with(name, FILE_OPEN, true, false, true)
    }

    pub(super) fn create_directory(&self, name: &OsStr, high_integrity: bool) -> io::Result<Self> {
        self.open_directory_with(name, FILE_CREATE, false, high_integrity, true)
    }

    fn open_directory_with(
        &self,
        name: &OsStr,
        disposition: u32,
        request_delete: bool,
        high_integrity: bool,
        enumerate: bool,
    ) -> io::Result<Self> {
        validate_component(name)?;
        let security = (disposition != FILE_OPEN)
            .then(|| SecurityDescriptor::directory(high_integrity))
            .transpose()?;
        let access = if enumerate {
            DIRECTORY_ACCESS
        } else {
            TRAVERSE_ACCESS
        } | if request_delete { DELETE } else { 0 };
        let file = open_relative(
            self.raw_handle(),
            name,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            disposition,
            FILE_ATTRIBUTE_DIRECTORY,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            security.as_ref(),
        )?;
        validate_handle(&file, ExpectedKind::Directory, false)?;
        Ok(Self {
            file,
            path: self.path.join(name),
        })
    }

    pub(super) fn create_file(
        &self,
        name: &OsStr,
        share_mode: u32,
        high_integrity: bool,
    ) -> io::Result<File> {
        self.create_file_with(name, share_mode, high_integrity, false)
    }

    pub(super) fn create_user_attachment_file(
        &self,
        name: &OsStr,
        share_mode: u32,
    ) -> io::Result<File> {
        self.create_file_with_security(
            name,
            share_mode,
            SecurityDescriptor::user_attachment_file()?,
            true,
        )
    }

    fn create_file_with(
        &self,
        name: &OsStr,
        share_mode: u32,
        high_integrity: bool,
        deletable: bool,
    ) -> io::Result<File> {
        self.create_file_with_security(
            name,
            share_mode,
            SecurityDescriptor::file(high_integrity)?,
            deletable,
        )
    }

    fn create_file_with_security(
        &self,
        name: &OsStr,
        share_mode: u32,
        security: SecurityDescriptor,
        deletable: bool,
    ) -> io::Result<File> {
        validate_component(name)?;
        let file = open_relative(
            self.raw_handle(),
            name,
            FILE_GENERIC_READ
                | FILE_GENERIC_WRITE
                | SYNCHRONIZE
                | if deletable { DELETE } else { 0 },
            share_mode,
            FILE_CREATE,
            FILE_ATTRIBUTE_NORMAL,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            Some(&security),
        )?;
        validate_handle(&file, ExpectedKind::RegularFile, true)?;
        Ok(file)
    }

    pub(super) fn open_file(&self, name: &OsStr) -> io::Result<File> {
        self.open_file_with(name, FILE_GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE)
    }

    pub(super) fn open_file_for_delete(&self, name: &OsStr) -> io::Result<File> {
        self.open_file_with(
            name,
            FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )
    }

    fn open_file_with(&self, name: &OsStr, access: u32, share_mode: u32) -> io::Result<File> {
        validate_component(name)?;
        let file = open_relative(
            self.raw_handle(),
            name,
            access,
            share_mode,
            FILE_OPEN,
            FILE_ATTRIBUTE_NORMAL,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
        )?;
        validate_handle(&file, ExpectedKind::RegularFile, true)?;
        Ok(file)
    }

    pub(super) fn entries(&self) -> io::Result<Vec<SecureDirectoryEntry>> {
        let mut entries = Vec::new();
        let mut restart_scan = true;
        loop {
            let mut storage = vec![0u64; DIRECTORY_QUERY_BUFFER_BYTES / size_of::<u64>()];
            let mut status_block = IO_STATUS_BLOCK::default();
            // Safety: the directory is a synchronous handle, the aligned buffer is writable for
            // its full byte length, and all optional asynchronous/filter pointers are null.
            let status = unsafe {
                NtQueryDirectoryFile(
                    self.raw_handle(),
                    null_mut(),
                    None,
                    null(),
                    &mut status_block,
                    storage.as_mut_ptr().cast(),
                    DIRECTORY_QUERY_BUFFER_BYTES as u32,
                    windows_sys::Wdk::Storage::FileSystem::FileDirectoryInformation,
                    false,
                    null(),
                    restart_scan,
                )
            };
            if status == STATUS_NO_MORE_FILES {
                break;
            }
            if status < 0 && status != STATUS_BUFFER_OVERFLOW {
                return Err(ntstatus_error(status));
            }
            let returned = status_block.Information;
            if returned == 0 {
                break;
            }
            let bytes = directory_query_bytes(&storage, returned)?;
            parse_directory_entries(bytes, &mut entries)?;
            restart_scan = false;
        }
        Ok(entries)
    }

    pub(super) fn delete_empty(self) -> io::Result<()> {
        mark_delete(&self.file)
    }
}

pub(super) fn mark_delete(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // Safety: the handle is live and `disposition` has the exact structure and size requested.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn rename_within_directory(
    file: &File,
    directory: &SecureDirectory,
    destination_name: &OsStr,
    replace_if_exists: bool,
) -> io::Result<()> {
    validate_component(destination_name)?;
    let wide = destination_name.encode_wide().collect::<Vec<_>>();
    let byte_length = wide
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long")
        })?;
    let fixed = offset_of!(FILE_RENAME_INFORMATION, FileName);
    let total = fixed
        .checked_add(byte_length as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rename buffer is too large"))?;
    let mut storage = vec![0u64; total.div_ceil(size_of::<u64>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // Safety: `storage` is suitably aligned and large enough for the fixed structure plus name.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = replace_if_exists;
        (*info).RootDirectory = directory.raw_handle();
        (*info).FileNameLength = byte_length;
        copy_nonoverlapping(
            wide.as_ptr(),
            storage.as_mut_ptr().cast::<u8>().add(fixed).cast::<u16>(),
            wide.len(),
        );
    }
    let mut status_block = IO_STATUS_BLOCK::default();
    // Safety: the source and root directory handles stay live for this synchronous native rename.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut status_block,
            storage.as_ptr().cast(),
            total as u32,
            FileRenameInformation,
        )
    };
    if status < 0 {
        Err(ntstatus_error(status))
    } else {
        Ok(())
    }
}

pub(super) fn random_bytes(output: &mut [u8]) -> io::Result<()> {
    let length = u32::try_from(output.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "random request is too large"))?;
    // Safety: the null algorithm handle selects the system RNG with the required flag.
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            output.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        Err(ntstatus_error(status))
    } else {
        Ok(())
    }
}

fn open_volume_or_share_root(path: &Path, enumerate: bool) -> io::Result<SecureDirectory> {
    let wide = nul_terminated(path.as_os_str())?;
    // Safety: `wide` is NUL-terminated and all optional pointers are null.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            if enumerate {
                DIRECTORY_ACCESS
            } else {
                TRAVERSE_ACCESS
            },
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // Safety: a successful CreateFileW call transfers one owned handle.
    let file = unsafe { File::from_raw_handle(handle as _) };
    validate_handle(&file, ExpectedKind::Directory, false)?;
    Ok(SecureDirectory {
        file,
        path: path.to_path_buf(),
    })
}

#[allow(clippy::too_many_arguments)]
fn open_relative(
    parent: HANDLE,
    name: &OsStr,
    desired_access: u32,
    share_access: u32,
    disposition: u32,
    file_attributes: u32,
    options: u32,
    security_descriptor: Option<&SecurityDescriptor>,
) -> io::Result<File> {
    let mut name = name.encode_wide().collect::<Vec<_>>();
    let byte_length = name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path component is too long"))?;
    let unicode = UNICODE_STRING {
        Length: byte_length,
        MaximumLength: byte_length,
        Buffer: name.as_mut_ptr(),
    };
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: &raw const unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security_descriptor.map_or(null(), SecurityDescriptor::as_ptr),
        SecurityQualityOfService: null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = null_mut();
    // Safety: all structures and the UTF-16 component remain live for this synchronous call.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &object_attributes,
            &mut status_block,
            null(),
            file_attributes,
            share_access,
            disposition,
            options,
            null(),
            0,
        )
    };
    if status < 0 {
        return Err(ntstatus_error(status));
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other(
            "NtCreateFile succeeded without returning a valid handle",
        ));
    }
    // Safety: successful NtCreateFile transfers one owned file handle.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

#[derive(Clone, Copy)]
enum ExpectedKind {
    Directory,
    RegularFile,
}

fn validate_handle(file: &File, expected: ExpectedKind, reject_hard_links: bool) -> io::Result<()> {
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    // Safety: `tag` is a writable structure of the exact requested type.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&raw mut tag).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reparse points are not allowed in diagnostic paths",
        ));
    }
    if tag.FileAttributes & FILE_ATTRIBUTE_DEVICE != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "device objects are not allowed in diagnostic paths",
        ));
    }
    let is_directory = tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if matches!(expected, ExpectedKind::Directory) != is_directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic path component has the wrong object type",
        ));
    }
    if reject_hard_links {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // Safety: `information` is a writable structure for a live file handle.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if information.nNumberOfLinks != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hard-linked files are not allowed in diagnostic storage",
            ));
        }
    }
    Ok(())
}

fn directory_query_bytes(storage: &[u64], initialized_len: usize) -> io::Result<&[u8]> {
    let capacity = storage
        .len()
        .checked_mul(size_of::<u64>())
        .ok_or_else(|| invalid_directory_data("directory query buffer size overflow"))?;
    if initialized_len > capacity {
        return Err(invalid_directory_data(
            "directory enumeration returned more bytes than the query buffer",
        ));
    }

    // Safety: `storage` is a live, initialized allocation and `initialized_len` was bounded by
    // its byte capacity above. The returned slice cannot outlive the borrowed storage.
    Ok(unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), initialized_len) })
}

fn parse_directory_entries(
    buffer: &[u8],
    output: &mut Vec<SecureDirectoryEntry>,
) -> io::Result<()> {
    let fixed = offset_of!(FILE_DIRECTORY_INFORMATION, FileName);
    let mut offset = 0usize;
    loop {
        let remaining = buffer
            .get(offset..)
            .ok_or_else(|| invalid_directory_data("directory record offset is out of bounds"))?;
        if remaining.len() < fixed {
            return Err(invalid_directory_data(
                "directory enumeration returned a truncated record",
            ));
        }

        let next = read_u32_field(
            remaining,
            offset_of!(FILE_DIRECTORY_INFORMATION, NextEntryOffset),
        )
        .ok_or_else(|| invalid_directory_data("directory record is missing its next offset"))?
            as usize;
        let record_len = if next == 0 {
            remaining.len()
        } else {
            if next < fixed || next > remaining.len() {
                return Err(invalid_directory_data(
                    "directory enumeration returned an invalid next-entry offset",
                ));
            }
            next
        };

        let name_bytes = read_u32_field(
            remaining,
            offset_of!(FILE_DIRECTORY_INFORMATION, FileNameLength),
        )
        .ok_or_else(|| invalid_directory_data("directory record is missing its name length"))?
            as usize;
        if !name_bytes.is_multiple_of(size_of::<u16>())
            || fixed
                .checked_add(name_bytes)
                .is_none_or(|name_end| name_end > record_len)
        {
            return Err(invalid_directory_data(
                "directory enumeration returned an invalid file name",
            ));
        }

        let name_end = fixed + name_bytes;
        let name_units = remaining[fixed..name_end]
            .chunks_exact(size_of::<u16>())
            .map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        let name = OsString::from_wide(&name_units);
        if name != OsStr::new(".") && name != OsStr::new("..") {
            validate_component(&name)?;
            output.push(SecureDirectoryEntry {
                name,
                attributes: read_u32_field(
                    remaining,
                    offset_of!(FILE_DIRECTORY_INFORMATION, FileAttributes),
                )
                .ok_or_else(|| {
                    invalid_directory_data("directory record is missing its attributes")
                })?,
                last_write_time: read_i64_field(
                    remaining,
                    offset_of!(FILE_DIRECTORY_INFORMATION, LastWriteTime),
                )
                .ok_or_else(|| {
                    invalid_directory_data("directory record is missing its write time")
                })?,
            });
        }
        if next == 0 {
            break;
        }
        offset += next;
    }
    Ok(())
}

fn read_u32_field(buffer: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; size_of::<u32>()] = buffer
        .get(offset..offset.checked_add(size_of::<u32>())?)?
        .try_into()
        .ok()?;
    Some(u32::from_ne_bytes(bytes))
}

fn read_i64_field(buffer: &[u8], offset: usize) -> Option<i64> {
    let bytes: [u8; size_of::<i64>()] = buffer
        .get(offset..offset.checked_add(size_of::<i64>())?)?
        .try_into()
        .ok()?;
    Some(i64::from_ne_bytes(bytes))
}

fn invalid_directory_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn split_absolute_path(path: &Path) -> io::Result<(PathBuf, Vec<OsString>)> {
    let mut components = path.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) => prefix.kind(),
        _ => return Err(invalid_absolute_path()),
    };
    let root = match prefix {
        Prefix::Disk(letter) => PathBuf::from(format!("{}:\\", char::from(letter))),
        Prefix::UNC(server, share) => {
            let mut root = OsString::from(r"\\");
            root.push(server);
            root.push("\\");
            root.push(share);
            root.push("\\");
            PathBuf::from(root)
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "verbatim and device namespace paths are not accepted for diagnostics",
            ));
        }
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(invalid_absolute_path());
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => {
                validate_component(name)?;
                names.push(name.to_os_string());
            }
            _ => return Err(invalid_absolute_path()),
        }
    }
    Ok((root, names))
}

fn validate_component(name: &OsStr) -> io::Result<()> {
    let units = name.encode_wide().collect::<Vec<_>>();
    if units.is_empty()
        || units == [b'.' as u16]
        || units == [b'.' as u16, b'.' as u16]
        || units
            .last()
            .is_some_and(|unit| *unit == u16::from(b'.') || *unit == u16::from(b' '))
        || units.iter().any(|unit| {
            *unit < 32
                || [
                    0,
                    u16::from(b'<'),
                    u16::from(b'>'),
                    u16::from(b':'),
                    u16::from(b'"'),
                    u16::from(b'/'),
                    u16::from(b'\\'),
                    u16::from(b'|'),
                    u16::from(b'?'),
                    u16::from(b'*'),
                ]
                .contains(unit)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic path contains an unsafe component",
        ));
    }
    Ok(())
}

fn nul_terminated(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide = value.encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic path contains NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn invalid_absolute_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "diagnostic paths must be absolute drive or UNC paths",
    )
}

fn ntstatus_error(status: i32) -> io::Error {
    // Safety: converting an NTSTATUS to its public Win32 error does not dereference pointers.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn directory(high_integrity: bool) -> io::Result<Self> {
        let user_sid = current_user_sid_string()?;
        Self::from_sddl(&directory_security_sddl(&user_sid, high_integrity))
    }

    fn file(high_integrity: bool) -> io::Result<Self> {
        let user_sid = current_user_sid_string()?;
        Self::from_sddl(&file_security_sddl(
            &user_sid,
            if high_integrity {
                Some(HIGH_INTEGRITY_FILE_LABEL)
            } else {
                None
            },
        ))
    }

    fn user_attachment_file() -> io::Result<Self> {
        let user_sid = current_user_sid_string()?;
        Self::from_sddl(&file_security_sddl(
            &user_sid,
            Some(MEDIUM_INTEGRITY_FILE_LABEL),
        ))
    }

    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = nul_terminated(OsStr::new(sddl))?;
        let mut descriptor = null_mut();
        // Safety: the input is NUL-terminated and the API allocates one LocalAlloc descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(descriptor))
        }
    }

    const fn as_ptr(&self) -> *const SECURITY_DESCRIPTOR {
        self.0.cast_const().cast()
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safety: the descriptor was allocated by the SDDL conversion API.
            unsafe {
                LocalFree(self.0 as _);
            }
        }
    }
}

fn directory_security_sddl(user_sid: &str, high_integrity: bool) -> String {
    let label = if high_integrity {
        HIGH_INTEGRITY_DIRECTORY_LABEL
    } else {
        ""
    };
    format!("O:{user_sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{user_sid}){label}")
}

fn file_security_sddl(user_sid: &str, integrity_label: Option<&str>) -> String {
    format!(
        "O:{user_sid}D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{user_sid}){}",
        integrity_label.unwrap_or_default()
    )
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token = null_mut();
    // Safety: the pseudo process handle is valid and `token` receives one owned handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: successful OpenProcessToken transferred one owned token handle whose documented
    // release function is CloseHandle; no other owner is retained.
    let token = unsafe { OwnedHandle::from_raw(token) }
        .ok_or_else(|| io::Error::other("OpenProcessToken returned an invalid handle"))?;

    let mut required = 0u32;
    // Safety: the sizing call writes only the required byte count.
    unsafe {
        GetTokenInformation(token.as_raw(), TokenUser, null_mut(), 0, &mut required);
    }
    if required < size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = vec![0u64; (required as usize).div_ceil(size_of::<u64>())];
    // Safety: the aligned allocation is at least `required` bytes and remains writable.
    if unsafe {
        GetTokenInformation(
            token.as_raw(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Safety: the successful query returned a complete TOKEN_USER in `storage`.
    let sid = unsafe { (*(storage.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    if sid.is_null() {
        return Err(io::Error::other("the current token has no user SID"));
    }

    let mut value = null_mut::<u16>();
    // Safety: the SID remains live in `storage`; the API returns one LocalAlloc string.
    if unsafe { ConvertSidToStringSidW(sid, &mut value) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0usize;
    // Safety: the conversion API returned a NUL-terminated UTF-16 string.
    unsafe {
        while *value.add(length) != 0 {
            length += 1;
        }
    }
    // Safety: the discovered range ends before the terminating NUL.
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(value, length) })
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "token SID is not valid UTF-16"));
    // Safety: `value` was allocated by ConvertSidToStringSidW.
    unsafe {
        LocalFree(value.cast());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_u32_field(buffer: &mut [u8], offset: usize, value: u32) {
        buffer[offset..offset + size_of::<u32>()].copy_from_slice(&value.to_ne_bytes());
    }

    fn write_i64_field(buffer: &mut [u8], offset: usize, value: i64) {
        buffer[offset..offset + size_of::<i64>()].copy_from_slice(&value.to_ne_bytes());
    }

    fn directory_record(name: &str, attributes: u32, last_write_time: i64) -> Vec<u8> {
        let fixed = offset_of!(FILE_DIRECTORY_INFORMATION, FileName);
        let name = name.encode_utf16().collect::<Vec<_>>();
        let mut record = vec![0u8; fixed + name.len() * size_of::<u16>()];
        write_u32_field(
            &mut record,
            offset_of!(FILE_DIRECTORY_INFORMATION, FileNameLength),
            (name.len() * size_of::<u16>()) as u32,
        );
        write_u32_field(
            &mut record,
            offset_of!(FILE_DIRECTORY_INFORMATION, FileAttributes),
            attributes,
        );
        write_i64_field(
            &mut record,
            offset_of!(FILE_DIRECTORY_INFORMATION, LastWriteTime),
            last_write_time,
        );
        for (index, unit) in name.into_iter().enumerate() {
            let start = fixed + index * size_of::<u16>();
            record[start..start + size_of::<u16>()].copy_from_slice(&unit.to_ne_bytes());
        }
        record
    }

    #[test]
    fn directory_query_length_is_bounded_by_typed_storage() {
        let storage = [0u64; 2];
        assert_eq!(
            directory_query_bytes(&storage, 7)
                .expect("in-range byte count should be accepted")
                .len(),
            7
        );
        assert_eq!(
            directory_query_bytes(&storage, 17)
                .expect_err("out-of-range byte count must be rejected")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn directory_parser_reads_a_typed_record_slice() {
        let record = directory_record("task.log", FILE_ATTRIBUTE_NORMAL, 42);
        let mut entries = Vec::new();
        parse_directory_entries(&record, &mut entries).expect("record should parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, OsStr::new("task.log"));
        assert_eq!(entries[0].attributes, FILE_ATTRIBUTE_NORMAL);
        assert_eq!(entries[0].last_write_time, 42);
    }

    #[test]
    fn directory_parser_rejects_a_name_crossing_the_record_boundary() {
        let mut record = directory_record("task.log", FILE_ATTRIBUTE_NORMAL, 42);
        write_u32_field(
            &mut record,
            offset_of!(FILE_DIRECTORY_INFORMATION, NextEntryOffset),
            offset_of!(FILE_DIRECTORY_INFORMATION, FileName) as u32,
        );
        let mut entries = Vec::new();
        assert_eq!(
            parse_directory_entries(&record, &mut entries)
                .expect_err("name must stay within its own record")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn directory_parser_rejects_a_truncated_header() {
        let fixed = offset_of!(FILE_DIRECTORY_INFORMATION, FileName);
        let truncated = vec![0u8; fixed - 1];
        let mut entries = Vec::new();
        assert_eq!(
            parse_directory_entries(&truncated, &mut entries)
                .expect_err("truncated header must be rejected")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn security_descriptors_name_the_token_user_instead_of_owner_rights() {
        let user_sid =
            current_user_sid_string().expect("current token user SID should be readable");
        let owner = format!("O:{user_sid}");
        let file_ace = format!("(A;;FA;;;{user_sid})");
        let directory_ace = format!("(A;OICI;FA;;;{user_sid})");

        let file = file_security_sddl(&user_sid, Some(MEDIUM_INTEGRITY_FILE_LABEL));
        assert!(file.starts_with(&owner));
        assert!(file.contains(&file_ace));
        assert!(!file.contains(";;;OW)"));
        assert!(file.ends_with(MEDIUM_INTEGRITY_FILE_LABEL));
        SecurityDescriptor::from_sddl(&file).expect("file descriptor should be valid SDDL");

        let directory = directory_security_sddl(&user_sid, true);
        assert!(directory.starts_with(&owner));
        assert!(directory.contains(&directory_ace));
        assert!(!directory.contains(";;;OW)"));
        assert!(directory.ends_with(HIGH_INTEGRITY_DIRECTORY_LABEL));
        SecurityDescriptor::from_sddl(&directory)
            .expect("directory descriptor should be valid SDDL");
    }
}
