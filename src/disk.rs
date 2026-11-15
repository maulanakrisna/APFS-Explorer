//! Akses disk mentah (\\.\PhysicalDriveN atau file image) + pencarian partisi APFS lewat GPT.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::rc::Rc;

/// Windows mewajibkan offset & panjang baca disk fisik kelipatan ukuran sektor.
/// 4096 aman untuk disk 512e maupun 4Kn.
const ALIGN: u64 = 4096;

/// GUID tipe partisi APFS (7C3457EF-0000-11AA-AA11-00306543ECAC) dalam byte on-disk GPT.
const APFS_GUID: [u8; 16] = [
    0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
];

#[derive(Clone)]
pub struct Device {
    file: Rc<File>,
    base: u64,
}

impl Device {
    pub fn new(file: Rc<File>, base: u64) -> Self {
        Device { file, base }
    }

    /// Device baru yang offset 0-nya digeser `offset` byte (untuk satu partisi).
    pub fn at(&self, offset: u64) -> Device {
        Device {
            file: self.file.clone(),
            base: self.base + offset,
        }
    }

    /// Membaca `len` byte dari `off` (relatif terhadap awal partisi), dengan penyelarasan sektor.
    pub fn read_at(&self, off: u64, len: usize) -> io::Result<Vec<u8>> {
        let abs = self.base + off;
        let start = abs & !(ALIGN - 1);
        let end = (abs + len as u64 + ALIGN - 1) & !(ALIGN - 1);
        let mut buf = vec![0u8; (end - start) as usize];
        let mut done = 0usize;
        while done < buf.len() {
            let n = self.file.seek_read(&mut buf[done..], start + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        let skip = (abs - start) as usize;
        if done < skip + len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pembacaan melewati akhir disk/image",
            ));
        }
        Ok(buf[skip..skip + len].to_vec())
    }
}

pub fn open_physical(n: u32) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(3) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(format!(r"\\.\PhysicalDrive{n}"))
}

pub fn open_image(path: &str) -> io::Result<File> {
    OpenOptions::new().read(true).share_mode(3).open(path)
}

pub struct Part {
    /// Offset awal partisi (byte) dari awal disk/image.
    pub offset: u64,
}

/// Mencari partisi APFS pada disk: lewat tabel GPT, atau (untuk image mentah) kontainer di offset 0.
pub fn apfs_partitions(dev: &Device) -> io::Result<Vec<Part>> {
    for ss in [512u64, 4096u64] {
        let Ok(h) = dev.read_at(ss, 512) else { continue };
        if &h[0..8] != b"EFI PART" {
            continue;
        }
        let entries_lba = u64::from_le_bytes(h[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(h[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(h[84..88].try_into().unwrap()) as usize;
        if n == 0 || n > 1024 || !(128..=1024).contains(&sz) {
            return Ok(vec![]);
        }
        let table = dev.read_at(entries_lba * ss, n * sz)?;
        let mut out = Vec::new();
        for i in 0..n {
            let e = &table[i * sz..(i + 1) * sz];
            if &e[0..16] == &APFS_GUID {
                let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
                out.push(Part { offset: first * ss });
            }
        }
        return Ok(out);
    }

    // Tanpa GPT: mungkin image kontainer APFS mentah.
    let b = dev.read_at(0, 4096)?;
    if &b[32..36] == b"NXSB" {
        return Ok(vec![Part { offset: 0 }]);
    }
    Ok(vec![])
}
