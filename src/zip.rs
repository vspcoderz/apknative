//! Minimal ZIP reader — just enough to pull entries out of an APK.
//!
//! An APK is an ordinary ZIP archive. We walk the central directory (which is
//! authoritative for entry sizes, unlike the local header when a data
//! descriptor is used) and inflate on demand.

use std::io::Read;

const EOCD_SIG: u32 = 0x0605_4b50;
const CD_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;

fn u16le(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}
fn u32le(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

pub struct Zip {
    data: Vec<u8>,
    cd_off: usize,
    count: u16,
}

#[derive(Clone, Copy)]
pub struct Entry {
    pub method: u16,
    pub comp_size: u32,
    pub uncomp_size: u32,
    pub local_off: usize,
}

impl Zip {
    pub fn open(data: Vec<u8>) -> Result<Self, String> {
        let n = data.len();
        if n < 22 {
            return Err("file is too small to be a ZIP".into());
        }
        // The EOCD lives at the very end, but a trailing comment can push it
        // back up to 64 KiB, so scan backwards across that window.
        let start = n.saturating_sub(65_557);
        let mut eocd = None;
        let mut i = n - 22;
        loop {
            if u32le(&data, i) == EOCD_SIG {
                eocd = Some(i);
                break;
            }
            if i == start {
                break;
            }
            i -= 1;
        }
        let eocd = eocd.ok_or("no end-of-central-directory record (not a ZIP/APK)")?;
        let cd_off = u32le(&data, eocd + 16) as usize;
        let count = u16le(&data, eocd + 10);
        Ok(Zip { data, cd_off, count })
    }

    /// Walk the central directory, calling `f` once per entry.
    pub fn entries<F: FnMut(&str, &Entry)>(&self, mut f: F) -> Result<(), String> {
        let d = &self.data;
        let mut p = self.cd_off;
        for _ in 0..self.count {
            if p + 46 > d.len() || u32le(d, p) != CD_SIG {
                return Err("corrupt central directory".into());
            }
            let method = u16le(d, p + 10);
            let comp_size = u32le(d, p + 20);
            let uncomp_size = u32le(d, p + 24);
            let name_len = u16le(d, p + 28) as usize;
            let extra_len = u16le(d, p + 30) as usize;
            let comment_len = u16le(d, p + 32) as usize;
            let local_off = u32le(d, p + 42) as usize;
            let name = String::from_utf8_lossy(&d[p + 46..p + 46 + name_len]).into_owned();
            f(
                &name,
                &Entry {
                    method,
                    comp_size,
                    uncomp_size,
                    local_off,
                },
            );
            p += 46 + name_len + extra_len + comment_len;
        }
        Ok(())
    }

    pub fn names(&self) -> Result<Vec<String>, String> {
        let mut v = Vec::new();
        self.entries(|n, _| v.push(n.to_string()))?;
        Ok(v)
    }

    pub fn find(&self, name: &str) -> Option<Entry> {
        let mut hit: Option<Entry> = None;
        let _ = self.entries(|n, e| {
            if n == name && hit.is_none() {
                hit = Some(Entry {
                    method: e.method,
                    comp_size: e.comp_size,
                    uncomp_size: e.uncomp_size,
                    local_off: e.local_off,
                });
            }
        });
        hit
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>, String> {
        let e = self.find(name).ok_or_else(|| format!("entry not found: {name}"))?;
        self.read_entry(&e)
    }

    pub fn read_entry(&self, e: &Entry) -> Result<Vec<u8>, String> {
        let d = &self.data;
        let lo = e.local_off;
        if lo + 30 > d.len() || u32le(d, lo) != LFH_SIG {
            return Err("corrupt local file header".into());
        }
        // The local header repeats the name/extra lengths. Its size fields can
        // be zero when a data descriptor is used, so trust the central
        // directory's sizes instead.
        let name_len = u16le(d, lo + 26) as usize;
        let extra_len = u16le(d, lo + 28) as usize;
        let start = lo + 30 + name_len + extra_len;
        let end = start + e.comp_size as usize;
        if end > d.len() {
            return Err("entry data runs past end of file".into());
        }
        let raw = &d[start..end];
        match e.method {
            0 => Ok(raw.to_vec()),
            8 => {
                let mut out = Vec::with_capacity(e.uncomp_size as usize);
                flate2::read::DeflateDecoder::new(raw)
                    .read_to_end(&mut out)
                    .map_err(|err| format!("inflate failed: {err}"))?;
                Ok(out)
            }
            m => Err(format!("unsupported compression method {m}")),
        }
    }
}