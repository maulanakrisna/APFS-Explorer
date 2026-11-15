//! Pembaca APFS read-only minimal: kontainer, object map, volume, B-tree FS, direktori & berkas.
//!
//! Belum didukung: volume terenkripsi (FileVault), berkas terkompresi (decmpfs),
//! snapshot, Fusion drive, validasi checksum Fletcher-64.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};
use std::rc::Rc;

use crate::disk::Device;

pub type Result<T> = io::Result<T>;

pub const ROOT_INO: u64 = 2;

const NXSB_MAGIC: u32 = 0x4253_584E; // "NXSB"
const APSB_MAGIC: u32 = 0x4253_5041; // "APSB"
const OID_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;

const T_INODE: u8 = 3;
const T_EXTENT: u8 = 8;
const T_DREC: u8 = 9;

fn bad<T>(m: impl Into<String>) -> Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, m.into()))
}

fn u16le(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |s| u16::from_le_bytes(s.try_into().unwrap()))
}
fn u32le(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn u64le(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

fn rtype(k: &[u8]) -> u8 {
    (u64le(k, 0) >> 60) as u8
}
fn roid(k: &[u8]) -> u64 {
    u64le(k, 0) & OID_MASK
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    Link,
    Other,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub ino: u64,
    pub kind: Kind,
    pub size: u64,
    /// Waktu modifikasi: nanodetik sejak 1970-01-01 UTC.
    pub mtime: u64,
}

// ---------------------------------------------------------------- B-tree node

struct Node {
    d: Vec<u8>,
    flags: u16,
    n: usize,
    toc: usize,
    keys: usize,
    vend: usize,
}

impl Node {
    fn parse(d: Vec<u8>) -> Result<Node> {
        if d.len() < 4096 {
            return bad("blok terlalu kecil");
        }
        let flags = u16le(&d, 32);
        let n = u32le(&d, 36) as usize;
        let toc = 56 + u16le(&d, 40) as usize;
        let keys = toc + u16le(&d, 42) as usize;
        let vend = if flags & 1 != 0 { d.len() - 40 } else { d.len() };
        if n > d.len() / 4 || keys > d.len() {
            return bad("node B-tree rusak");
        }
        Ok(Node { d, flags, n, toc, keys, vend })
    }

    fn leaf(&self) -> bool {
        self.flags & 2 != 0
    }

    /// `kl`/`vl` hanya dipakai pada node ukuran-tetap (omap).
    fn kv(&self, i: usize, kl: usize, vl: usize) -> Result<(&[u8], &[u8])> {
        let d = &self.d;
        let (ko, klen, vo, vlen) = if self.flags & 4 != 0 {
            let e = self.toc + i * 4;
            (u16le(d, e) as usize, kl, u16le(d, e + 2) as usize, vl)
        } else {
            let e = self.toc + i * 8;
            (
                u16le(d, e) as usize,
                u16le(d, e + 2) as usize,
                u16le(d, e + 4) as usize,
                u16le(d, e + 6) as usize,
            )
        };
        let ks = self.keys + ko;
        let key = d.get(ks..ks + klen);
        let val = self.vend.checked_sub(vo).and_then(|s| d.get(s..s + vlen));
        match (key, val) {
            (Some(k), Some(v)) => Ok((k, v)),
            _ => bad("entri B-tree rusak"),
        }
    }
}

// ------------------------------------------------------------------ Container

pub struct Container {
    dev: Device,
    bs: usize,
    xid: u64,
    omap_tree: u64,
    fs_oids: Vec<u64>,
}

impl Container {
    pub fn open(dev: Device) -> Result<Rc<Container>> {
        let first = dev.read_at(0, 4096)?;
        if u32le(&first, 32) != NXSB_MAGIC {
            return bad("bukan kontainer APFS (magic NXSB tidak ditemukan)");
        }
        let bs = u32le(&first, 36) as usize;
        if !(4096..=65536).contains(&bs) || !bs.is_power_of_two() {
            return bad("ukuran blok APFS tidak valid");
        }
        let mut sb = dev.read_at(0, bs)?;

        // Blok 0 bisa usang; superblok terbaru ada di area deskriptor checkpoint.
        let desc_blocks = u32le(&sb, 104);
        let desc_base = u64le(&sb, 112);
        if (desc_base as i64) > 0 && desc_blocks & 0x8000_0000 == 0 {
            for i in 0..desc_blocks.min(4096) as u64 {
                let Ok(b) = dev.read_at((desc_base + i) * bs as u64, bs) else { continue };
                if u32le(&b, 24) & 0xFFFF == 1
                    && u32le(&b, 32) == NXSB_MAGIC
                    && u64le(&b, 16) > u64le(&sb, 16)
                {
                    sb = b;
                }
            }
        }

        let xid = u64le(&sb, 16);
        let max_fs = (u32le(&sb, 180) as usize).min(100);
        let fs_oids: Vec<u64> = (0..max_fs)
            .map(|i| u64le(&sb, 184 + i * 8))
            .filter(|&o| o != 0)
            .collect();

        let cont = Container { dev, bs, xid, omap_tree: 0, fs_oids };
        let omap = cont.read_block(u64le(&sb, 160))?;
        let omap_tree = u64le(&omap, 48);
        Ok(Rc::new(Container { omap_tree, ..cont }))
    }

    fn read_block(&self, paddr: u64) -> Result<Vec<u8>> {
        self.dev.read_at(paddr * self.bs as u64, self.bs)
    }

    /// Mencari paddr untuk `oid` virtual dengan xid terbesar yang <= `xid`.
    fn omap_lookup(&self, tree: u64, oid: u64, xid: u64) -> Result<Option<u64>> {
        let mut blk = tree;
        for _ in 0..16 {
            let node = Node::parse(self.read_block(blk)?)?;
            if node.leaf() {
                let mut best: Option<(u64, u64)> = None;
                for i in 0..node.n {
                    let (k, v) = node.kv(i, 16, 16)?;
                    let (koid, kxid) = (u64le(k, 0), u64le(k, 8));
                    let deleted = u32le(v, 0) & 1 != 0;
                    if koid == oid && kxid <= xid && !deleted && best.map_or(true, |b| kxid > b.0) {
                        best = Some((kxid, u64le(v, 8)));
                    }
                }
                return Ok(best.map(|b| b.1));
            }
            let mut child = None;
            for i in 0..node.n {
                let (k, v) = node.kv(i, 16, 8)?;
                if (u64le(k, 0), u64le(k, 8)) <= (oid, xid) {
                    child = Some(u64le(v, 0));
                } else {
                    break;
                }
            }
            match child {
                Some(c) => blk = c,
                None => return Ok(None),
            }
        }
        bad("object map terlalu dalam")
    }

    pub fn volumes(self: &Rc<Self>) -> Result<Vec<Volume>> {
        let mut out = Vec::new();
        for &oid in &self.fs_oids {
            let Some(paddr) = self.omap_lookup(self.omap_tree, oid, self.xid)? else { continue };
            let b = self.read_block(paddr)?;
            if u32le(&b, 32) != APSB_MAGIC {
                continue;
            }
            let omap = self.read_block(u64le(&b, 128))?;
            let raw = &b[704..960];
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            let name = String::from_utf8_lossy(&raw[..end]).into_owned();
            out.push(Volume {
                cont: self.clone(),
                name: if name.is_empty() { "(tanpa nama)".into() } else { name },
                omap_tree: u64le(&omap, 48),
                root_virt: u64le(&b, 136),
                xid: u64le(&b, 16),
                encrypted: u64le(&b, 264) & 1 == 0,
                cache: RefCell::new(HashMap::new()),
            });
        }
        Ok(out)
    }
}

// --------------------------------------------------------------------- Volume

pub struct Volume {
    cont: Rc<Container>,
    pub name: String,
    omap_tree: u64,
    root_virt: u64,
    xid: u64,
    encrypted: bool,
    cache: RefCell<HashMap<u64, u64>>,
}

struct Inode {
    mode: u16,
    size: u64,
    mtime: u64,
    private_id: u64,
    bsd_flags: u32,
}

impl Volume {
    fn check(&self) -> Result<()> {
        if self.encrypted {
            return bad("Volume ini terenkripsi (FileVault) — belum didukung");
        }
        Ok(())
    }

    fn resolve(&self, virt: u64) -> Result<u64> {
        if let Some(&p) = self.cache.borrow().get(&virt) {
            return Ok(p);
        }
        match self.cont.omap_lookup(self.omap_tree, virt, self.xid)? {
            Some(p) => {
                self.cache.borrow_mut().insert(virt, p);
                Ok(p)
            }
            None => bad(format!("oid {virt} tidak ada di object map")),
        }
    }

    /// Semua record FS-tree untuk object id `oid` (inode, drec, extent, xattr, ...).
    fn records(&self, oid: u64) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.scan(self.root_virt, oid, &mut out, 0)?;
        Ok(out)
    }

    fn scan(&self, virt: u64, oid: u64, out: &mut Vec<(Vec<u8>, Vec<u8>)>, depth: u32) -> Result<()> {
        if depth > 32 {
            return bad("B-tree terlalu dalam");
        }
        let node = Node::parse(self.cont.read_block(self.resolve(virt)?)?)?;
        if node.leaf() {
            for i in 0..node.n {
                let (k, v) = node.kv(i, 0, 0)?;
                if roid(k) == oid {
                    out.push((k.to_vec(), v.to_vec()));
                }
            }
        } else {
            for i in 0..node.n {
                let (k, v) = node.kv(i, 0, 0)?;
                let next = if i + 1 < node.n { roid(node.kv(i + 1, 0, 0)?.0) } else { u64::MAX };
                if roid(k) <= oid && next >= oid {
                    self.scan(u64le(v, 0), oid, out, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn inode(&self, ino: u64) -> Result<Inode> {
        for (k, v) in self.records(ino)? {
            if rtype(&k) == T_INODE {
                let size = if v.len() > 92 { dstream_size(&v[92..]).unwrap_or(0) } else { 0 };
                return Ok(Inode {
                    mode: u16le(&v, 80),
                    size,
                    mtime: u64le(&v, 24),
                    private_id: u64le(&v, 8),
                    bsd_flags: u32le(&v, 68),
                });
            }
        }
        bad(format!("inode {ino} tidak ditemukan"))
    }

    pub fn read_dir(&self, dir: u64) -> Result<Vec<Entry>> {
        self.check()?;
        let mut out = Vec::new();
        for (k, v) in self.records(dir)? {
            if rtype(&k) != T_DREC {
                continue;
            }
            let Some(name) = drec_name(&k) else { continue };
            let child = u64le(&v, 0);
            let kind = match u16le(&v, 16) & 0xF {
                4 => Kind::Dir,
                8 => Kind::File,
                10 => Kind::Link,
                14 => continue, // whiteout
                _ => Kind::Other,
            };
            // Satu scan per anak; cukup untuk direktori berukuran wajar.
            let (size, mtime) = self.inode(child).map(|i| (i.size, i.mtime)).unwrap_or((0, 0));
            out.push(Entry { name, ino: child, kind, size, mtime });
        }
        Ok(out)
    }

    pub fn read_file(&self, ino: u64, out: &mut dyn Write) -> Result<()> {
        self.check()?;
        let inode = self.inode(ino)?;
        if inode.mode & 0o170000 != 0o100000 {
            return bad("bukan berkas biasa");
        }
        if inode.bsd_flags & 0x20 != 0 {
            return bad("Berkas terkompresi APFS (decmpfs) belum didukung");
        }

        let mut exts: Vec<(u64, u64, u64)> = Vec::new(); // (logical, length, physical block)
        for (k, v) in self.records(inode.private_id)? {
            if rtype(&k) == T_EXTENT {
                exts.push((u64le(&k, 8), u64le(&v, 0) & 0x00FF_FFFF_FFFF_FFFF, u64le(&v, 8)));
            }
        }
        exts.sort();

        let bs = self.cont.bs as u64;
        let size = inode.size;
        let mut pos = 0u64;
        for (logical, len, phys) in exts {
            if logical >= size {
                break;
            }
            if logical > pos {
                write_zeros(out, logical - pos)?;
            }
            let n = len.min(size - logical);
            if phys == 0 {
                write_zeros(out, n)?;
            } else {
                let mut done = 0u64;
                while done < n {
                    let c = (n - done).min(1 << 20);
                    let buf = self.cont.dev.read_at(phys * bs + done, c as usize)?;
                    out.write_all(&buf)?;
                    done += c;
                }
            }
            pos = logical + n;
        }
        if pos < size {
            write_zeros(out, size - pos)?;
        }
        out.flush()
    }
}

fn write_zeros(out: &mut dyn Write, mut n: u64) -> io::Result<()> {
    let z = vec![0u8; 1 << 16];
    while n > 0 {
        let c = n.min(z.len() as u64) as usize;
        out.write_all(&z[..c])?;
        n -= c as u64;
    }
    Ok(())
}

/// Nama dari key DIR_REC: varian ber-hash (u32: panjang 10 bit + hash 22 bit) atau lama (u16).
fn drec_name(k: &[u8]) -> Option<String> {
    let hashed_len = (u32le(k, 8) & 0x3FF) as usize;
    let raw = if k.len() >= 12 && 12 + hashed_len == k.len() {
        &k[12..]
    } else {
        let l = u16le(k, 8) as usize;
        k.get(10..10 + l)?
    };
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// Mengambil ukuran dari xfield DSTREAM (tipe 8) pada blob xfields inode.
fn dstream_size(x: &[u8]) -> Option<u64> {
    let n = u16le(x, 0) as usize;
    let mut off = 4 + 4 * n;
    for i in 0..n {
        let t = *x.get(4 + 4 * i)?;
        let sz = u16le(x, 4 + 4 * i + 2) as usize;
        if t == 8 {
            return Some(u64le(x, off));
        }
        off += (sz + 7) & !7;
    }
    None
}
