// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 诊断包归档
//
//   文件:       src/infrastructure/diagnostics/archive.rs
//
//   日期:       2026年07月27日
//   环境:       Fedora Linux 45 x86_64；Linux 内核 7.2.0-0.rc4.260725g0ce37745d4bf.39.fc45.x86_64；Rust 1.97.1；MinGW GCC 16.1.1；Wine 11.14 (Staging)
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 以 ZIP “store”模式流式写入诊断包。
//!
//! 诊断文件可能接近日志保留上限，归档器不能把整个会话读入内存。这里使用带数据
//! 描述符的 ZIP32 条目，一边复制一边计算 CRC32，并在结束时统一写中央目录。

use std::fs::File;
use std::io::{self, Read, Write};

use crc32fast::Hasher;

const LOCAL_FILE_HEADER_SIGNATURE: u32 = 0x0403_4B50;
const DATA_DESCRIPTOR_SIGNATURE: u32 = 0x0807_4B50;
const CENTRAL_DIRECTORY_SIGNATURE: u32 = 0x0201_4B50;
const END_OF_CENTRAL_DIRECTORY_SIGNATURE: u32 = 0x0605_4B50;
const VERSION_NEEDED: u16 = 20;
const VERSION_MADE_BY: u16 = 20;
const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
const FLAG_UTF8: u16 = 1 << 11;
const STORED_METHOD: u16 = 0;
const COPY_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug)]
struct CentralEntry {
    name: Vec<u8>,
    crc32: u32,
    size: u32,
    local_header_offset: u32,
}

pub(super) struct StoredZipWriter<'file> {
    file: &'file mut File,
    position: u64,
    entries: Vec<CentralEntry>,
}

impl<'file> StoredZipWriter<'file> {
    pub(super) fn from_file(file: &'file mut File) -> Self {
        Self {
            file,
            position: 0,
            entries: Vec::new(),
        }
    }

    pub(super) fn add_open_file_prefix(
        &mut self,
        archive_name: &str,
        source: &mut File,
        length: u64,
    ) -> io::Result<()> {
        let mut prefix = source.take(length);
        self.add_reader(archive_name, &mut prefix)?;
        if prefix.limit() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "diagnostic source was truncated while it was being archived",
            ));
        }
        Ok(())
    }

    pub(super) fn add_bytes(&mut self, archive_name: &str, bytes: &[u8]) -> io::Result<()> {
        self.add_reader(archive_name, &mut io::Cursor::new(bytes))
    }

    fn add_reader(&mut self, archive_name: &str, reader: &mut impl Read) -> io::Result<()> {
        validate_archive_name(archive_name)?;
        if self.entries.len() == usize::from(u16::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "diagnostic archive has too many entries",
            ));
        }

        let name = archive_name.as_bytes().to_vec();
        let name_length = u16::try_from(name.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry name is too long")
        })?;
        let local_header_offset = self.position_u32()?;
        let flags = FLAG_DATA_DESCRIPTOR | FLAG_UTF8;

        self.write_u32(LOCAL_FILE_HEADER_SIGNATURE)?;
        self.write_u16(VERSION_NEEDED)?;
        self.write_u16(flags)?;
        self.write_u16(STORED_METHOD)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u32(0)?;
        self.write_u32(0)?;
        self.write_u32(0)?;
        self.write_u16(name_length)?;
        self.write_u16(0)?;
        self.write_all(&name)?;

        let mut hasher = Hasher::new();
        let mut size = 0u64;
        let mut buffer = [0u8; COPY_BUFFER_SIZE];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            self.write_all(&buffer[..count])?;
            size = size
                .checked_add(count as u64)
                .ok_or_else(zip32_size_error)?;
            if size > u64::from(u32::MAX) {
                return Err(zip32_size_error());
            }
        }

        let size = u32::try_from(size).map_err(|_| zip32_size_error())?;
        let crc32 = hasher.finalize();
        self.write_u32(DATA_DESCRIPTOR_SIGNATURE)?;
        self.write_u32(crc32)?;
        self.write_u32(size)?;
        self.write_u32(size)?;
        self.entries.push(CentralEntry {
            name,
            crc32,
            size,
            local_header_offset,
        });
        Ok(())
    }

    pub(super) fn finish(mut self) -> io::Result<()> {
        let central_offset = self.position_u32()?;
        for index in 0..self.entries.len() {
            let (name, crc32, size, local_header_offset) = {
                let entry = &self.entries[index];
                (
                    entry.name.clone(),
                    entry.crc32,
                    entry.size,
                    entry.local_header_offset,
                )
            };
            let name_length = u16::try_from(name.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry name is too long")
            })?;
            self.write_u32(CENTRAL_DIRECTORY_SIGNATURE)?;
            self.write_u16(VERSION_MADE_BY)?;
            self.write_u16(VERSION_NEEDED)?;
            self.write_u16(FLAG_DATA_DESCRIPTOR | FLAG_UTF8)?;
            self.write_u16(STORED_METHOD)?;
            self.write_u16(0)?;
            self.write_u16(0)?;
            self.write_u32(crc32)?;
            self.write_u32(size)?;
            self.write_u32(size)?;
            self.write_u16(name_length)?;
            self.write_u16(0)?;
            self.write_u16(0)?;
            self.write_u16(0)?;
            self.write_u16(0)?;
            self.write_u32(0)?;
            self.write_u32(local_header_offset)?;
            self.write_all(&name)?;
        }

        let central_end = self.position_u32()?;
        let central_size = central_end
            .checked_sub(central_offset)
            .ok_or_else(zip32_size_error)?;
        let entry_count = u16::try_from(self.entries.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "diagnostic archive has too many entries",
            )
        })?;
        self.write_u32(END_OF_CENTRAL_DIRECTORY_SIGNATURE)?;
        self.write_u16(0)?;
        self.write_u16(0)?;
        self.write_u16(entry_count)?;
        self.write_u16(entry_count)?;
        self.write_u32(central_size)?;
        self.write_u32(central_offset)?;
        self.write_u16(0)?;
        self.file.flush()?;
        self.file.sync_all()
    }

    fn position_u32(&self) -> io::Result<u32> {
        u32::try_from(self.position).map_err(|_| zip32_size_error())
    }

    fn write_u16(&mut self, value: u16) -> io::Result<()> {
        self.write_all(&value.to_le_bytes())
    }

    fn write_u32(&mut self, value: u32) -> io::Result<()> {
        self.write_all(&value.to_le_bytes())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(zip32_size_error)?;
        Ok(())
    }
}

fn validate_archive_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.split('/').any(|part| part == "..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZIP entry name must be a relative forward-slash path",
        ));
    }
    Ok(())
}

fn zip32_size_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "diagnostic archive exceeds the ZIP32 size limit",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_archive_path() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "taskmgr-rs-archive-test-{}-{unique}.zip",
            std::process::id()
        ))
    }

    fn create_archive_file(path: &Path) -> File {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("archive file should be created exclusively")
    }

    #[test]
    fn writes_streaming_zip_with_central_directory() {
        let path = temporary_archive_path();
        let mut file = create_archive_file(&path);
        let mut archive = StoredZipWriter::from_file(&mut file);
        archive
            .add_bytes("manifest.json", br#"{"schema":1}"#)
            .expect("entry should be written");
        archive
            .add_bytes("sessions/session-a/log.jsonl", b"one\ntwo\n")
            .expect("second entry should be written");
        archive.finish().expect("archive should finish");

        let bytes = fs::read(&path).expect("archive should be readable");
        assert!(bytes.starts_with(&LOCAL_FILE_HEADER_SIGNATURE.to_le_bytes()));
        assert!(
            bytes
                .windows(4)
                .any(|window| window == CENTRAL_DIRECTORY_SIGNATURE.to_le_bytes())
        );
        assert!(
            bytes
                .windows(4)
                .any(|window| window == END_OF_CENTRAL_DIRECTORY_SIGNATURE.to_le_bytes())
        );
        assert!(
            bytes
                .windows(b"manifest.json".len())
                .any(|window| window == b"manifest.json")
        );
        fs::remove_file(path).expect("test archive should be removable");
    }

    #[test]
    fn rejects_parent_and_backslash_entry_names() {
        let path = temporary_archive_path();
        let mut file = create_archive_file(&path);
        let mut archive = StoredZipWriter::from_file(&mut file);
        assert!(archive.add_bytes("../secret", b"x").is_err());
        assert!(archive.add_bytes("folder\\secret", b"x").is_err());
        drop(archive);
        fs::remove_file(path).expect("test archive should be removable");
    }

    #[test]
    fn file_prefix_freezes_a_live_log_at_its_flushed_length() {
        let source = temporary_archive_path().with_extension("jsonl");
        let archive_path = temporary_archive_path();
        fs::write(&source, b"complete line\nlater line\n").expect("source should be written");
        let mut source_file = File::open(&source).expect("source should open");
        let mut archive_file = create_archive_file(&archive_path);
        let mut archive = StoredZipWriter::from_file(&mut archive_file);
        archive
            .add_open_file_prefix("session/log.jsonl", &mut source_file, 14)
            .expect("flushed prefix should be archived");
        archive.finish().expect("archive should finish");

        let bytes = fs::read(&archive_path).expect("archive should be readable");
        assert!(
            bytes
                .windows(b"complete line\n".len())
                .any(|window| window == b"complete line\n")
        );
        assert!(
            !bytes
                .windows(b"later line".len())
                .any(|window| window == b"later line")
        );
        fs::remove_file(source).expect("source should be removable");
        fs::remove_file(archive_path).expect("archive should be removable");
    }
}
