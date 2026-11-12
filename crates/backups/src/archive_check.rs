//! Streaming checks for the two backup formats; no files are extracted.
use anyhow::{Context as _, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::{Component, Path},
    time::{Duration, Instant},
};
use tokio::sync::watch;

fn cancelled(cancel: &watch::Receiver<bool>) -> Result<()> {
    ensure!(!*cancel.borrow(), "Backup verification cancelled");
    Ok(())
}
// Hash the compressed stream as the decoder reads it: no second disk pass.
struct CheckedReader<R, F> {
    inner: R,
    hash: Sha256,
    bytes: u64,
    reported: Instant,
    progress: F,
}
impl<R: Read, F: FnMut(u64)> Read for CheckedReader<R, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buffer)?;
        self.hash.update(&buffer[..n]);
        self.bytes += n as u64;
        if self.reported.elapsed() >= Duration::from_secs(1) {
            (self.progress)(self.bytes);
            self.reported = Instant::now();
        }
        Ok(n)
    }
}

pub fn verify(path: &Path, index: usize, cancel: &watch::Receiver<bool>) -> Result<String> {
    verify_with_progress(path, index, cancel, |_| {})
}

pub fn verify_with_progress(
    path: &Path,
    index: usize,
    cancel: &watch::Receiver<bool>,
    mut progress: impl FnMut(u64),
) -> Result<String> {
    cancelled(cancel)?;
    progress(0);
    let input = CheckedReader {
        inner: File::open(path)?,
        hash: Sha256::new(),
        bytes: 0,
        reported: Instant::now(),
        progress,
    };
    let mut reader = zstd::stream::read::Decoder::new(input).context("Invalid zstd backup")?;
    if index == 0 {
        check_tar(&mut reader, cancel)?;
    } else {
        check_xbstream(&mut reader, cancel)?;
    }
    // Consume every compressed frame and checksum, including archive padding.
    drain(&mut reader, u64::MAX, None, cancel)?;
    cancelled(cancel)?;
    let mut input = reader.finish().into_inner();
    (input.progress)(input.bytes);
    cancelled(cancel)?;
    Ok(format!("{:x}", input.hash.finalize()))
}
fn safe_path(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("Invalid archive path")?;
    ensure!(!text.contains(['\\', '\0']), "Unsafe archive path");
    let mut parts = Vec::new();
    for component in Path::new(text).components() {
        match component {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => anyhow::bail!("Unsafe archive path"),
        }
    }
    ensure!(!parts.is_empty(), "Empty archive path");
    Ok(parts.join("/"))
}
fn check_tar(reader: &mut impl Read, cancel: &watch::Receiver<bool>) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut manifest = false;
    let mut metadata = false;
    for entry in archive.entries().context("Invalid ClickHouse TAR")? {
        cancelled(cancel)?;
        let mut entry = entry.context("Damaged ClickHouse TAR entry")?;
        if entry.header().entry_type().is_dir() && matches!(&*entry.path_bytes(), b"." | b"./") {
            continue;
        }
        let name = safe_path(&entry.path_bytes())?;
        if entry.header().entry_type().is_file() {
            manifest |= name == ".backup" && entry.size() > 0;
            metadata |= name.starts_with("metadata/") && entry.size() > 0;
            let expected = entry.size();
            ensure!(
                drain(&mut entry, u64::MAX, None, cancel)? == expected,
                "Truncated TAR file"
            );
        } else {
            ensure!(
                entry.header().entry_type().is_dir(),
                "Unsupported ClickHouse archive entry"
            );
        }
    }
    ensure!(
        manifest && metadata,
        "Archive lacks ClickHouse backup metadata"
    );
    Ok(())
}
fn number<const N: usize>(reader: &mut impl Read) -> Result<[u8; N]> {
    let mut b = [0; N];
    reader
        .read_exact(&mut b)
        .context("Truncated xbstream record")?;
    Ok(b)
}
fn drain(
    reader: &mut impl Read,
    mut bytes: u64,
    mut hash: Option<&mut crc32fast::Hasher>,
    cancel: &watch::Receiver<bool>,
) -> Result<u64> {
    let mut buf = [0u8; 64 * 1024];
    let mut read = 0;
    while bytes > 0 {
        cancelled(cancel)?;
        let limit = bytes.min(buf.len() as u64) as usize;
        let n = reader.read(&mut buf[..limit])?;
        if n == 0 {
            break;
        }
        if let Some(hash) = hash.as_mut() {
            hash.update(&buf[..n]);
        }
        bytes -= n as u64;
        read += n as u64;
    }
    Ok(read)
}
// Format reference (independent parser): Percona xbstream.h / xbstream_read.cc,
// https://github.com/percona/percona-xtrabackup/tree/8.4/storage/innobase/xtrabackup/src
fn check_xbstream(reader: &mut impl Read, cancel: &watch::Receiver<bool>) -> Result<()> {
    let mut open: BTreeMap<String, Vec<(u64, u64)>> = BTreeMap::new();
    let mut closed = BTreeMap::new();
    loop {
        cancelled(cancel)?;
        let mut header = [0u8; 14];
        if reader.read(&mut header[..1])? == 0 {
            break;
        }
        reader
            .read_exact(&mut header[1..])
            .context("Truncated xbstream header")?;
        ensure!(&header[..8] == b"XBSTCK01", "Invalid xbstream signature");
        let kind = header[9];
        ensure!(
            matches!(kind, b'P' | b'S' | b'E'),
            "Unsupported xbstream record type"
        );
        let len = u32::from_le_bytes(header[10..14].try_into()?) as usize;
        ensure!(len > 0 && len < 4096, "Invalid xbstream path length");
        let mut name = vec![0; len];
        reader.read_exact(&mut name)?;
        let name = safe_path(&name)?;
        ensure!(
            !closed.contains_key(&name),
            "Duplicate closed xbstream file"
        );
        if kind == b'E' {
            let mut spans = open.remove(&name).unwrap_or_default();
            spans.sort_unstable();
            let mut end = 0;
            for (start, next) in spans {
                ensure!(start == end, "Missing or overlapping xbstream blocks");
                end = next;
            }
            closed.insert(name, end);
            continue;
        }
        let sparse = if kind == b'S' {
            u32::from_le_bytes(number(reader)?)
        } else {
            0
        };
        ensure!(sparse < 1_000_000, "Invalid sparse map size");
        let length = u64::from_le_bytes(number(reader)?);
        let offset = u64::from_le_bytes(number(reader)?);
        let checksum = u32::from_le_bytes(number(reader)?);
        let mut hash = crc32fast::Hasher::new();
        let mut logical = 0u64;
        let mut payload = 0u64;
        for _ in 0..sparse {
            let map: [u8; 8] = number(reader)?;
            hash.update(&map);
            let skip = u32::from_le_bytes(map[..4].try_into()?) as u64;
            let len = u32::from_le_bytes(map[4..].try_into()?) as u64;
            logical = logical
                .checked_add(skip)
                .and_then(|v| v.checked_add(len))
                .context("Sparse map overflow")?;
            payload = payload.checked_add(len).context("Sparse map overflow")?;
        }
        if kind == b'S' {
            ensure!(payload == length, "Invalid sparse payload size");
        } else {
            logical = length;
        }
        ensure!(
            drain(reader, length, Some(&mut hash), cancel)? == length,
            "Truncated xbstream payload"
        );
        ensure!(hash.finalize() == checksum, "xbstream checksum mismatch");
        let end = offset
            .checked_add(logical)
            .context("xbstream offset overflow")?;
        open.entry(name).or_default().push((offset, end));
    }
    ensure!(open.is_empty(), "Unfinished xbstream files");
    for required in ["xtrabackup_info", "xtrabackup_checkpoints", "ibdata1"] {
        ensure!(
            closed.get(required).is_some_and(|length| *length > 0),
            "Backup lacks {required}"
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    pub fn tar_bytes() -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, data) in [
            (".backup", "<backup/>"),
            ("metadata/test/t.sql", "CREATE TABLE t (x UInt8) ENGINE=Log"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o600);
            header.set_size(data.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, path, data.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap()
    }
    pub fn xb_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        for name in ["ibdata1", "xtrabackup_info", "xtrabackup_checkpoints"] {
            let data = b"fixture data";
            bytes.extend_from_slice(b"XBSTCK01\0P");
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&0u64.to_le_bytes());
            bytes.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
            bytes.extend_from_slice(data);
            bytes.extend_from_slice(b"XBSTCK01\0E");
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
        }
        bytes
    }
    pub fn compress(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 1).unwrap()
    }
    #[test]
    fn complete_archives_are_accepted_and_truncation_or_bad_checksums_rejected() {
        let (_, cancel) = watch::channel(false);
        assert!(check_tar(&mut &tar_bytes()[..], &cancel).is_ok());
        let xb = xb_bytes();
        assert!(check_xbstream(&mut &xb[..], &cancel).is_ok());
        assert!(check_xbstream(&mut &xb[..xb.len() - 1], &cancel).is_err());
        let mut corrupt = xb.clone();
        let position = corrupt
            .windows(12)
            .position(|b| b == b"fixture data")
            .unwrap();
        corrupt[position] ^= 1;
        assert!(check_xbstream(&mut &corrupt[..], &cancel).is_err());
        assert!(check_tar(&mut &tar_bytes()[..520], &cancel).is_err());
        assert!(check_tar(&mut &b"invalid archive"[..], &cancel).is_err());
    }
    #[test]
    fn sparse_records_include_map_bytes_in_checksum() {
        let (_, cancel) = watch::channel(false);
        let original = xb_bytes();
        let name = b"ibdata1";
        let data = b"fixture data";
        let map = [8u32.to_le_bytes(), (data.len() as u32).to_le_bytes()].concat();
        let checksum = crc32fast::hash(&[map.as_slice(), data].concat());
        let mut sparse = b"XBSTCK01\0S".to_vec();
        sparse.extend_from_slice(&(name.len() as u32).to_le_bytes());
        sparse.extend_from_slice(name);
        sparse.extend_from_slice(&1u32.to_le_bytes());
        sparse.extend_from_slice(&(data.len() as u64).to_le_bytes());
        sparse.extend_from_slice(&0u64.to_le_bytes());
        sparse.extend_from_slice(&checksum.to_le_bytes());
        let map_position = sparse.len();
        sparse.extend_from_slice(&map);
        sparse.extend_from_slice(data);
        sparse.extend_from_slice(&original[14 + name.len() + 8 + 8 + 4 + data.len()..]);
        assert!(check_xbstream(&mut &sparse[..], &cancel).is_ok());
        sparse[map_position] ^= 1;
        assert!(check_xbstream(&mut &sparse[..], &cancel).is_err());
    }
    #[test]
    fn zstd_damage_and_cancellation_are_rejected() -> Result<()> {
        let path = std::env::temp_dir().join(format!("sandbox-verify-{}.zst", std::process::id()));
        let (_, cancel) = watch::channel(false);
        let bytes = compress(&tar_bytes());
        std::fs::write(&path, &bytes)?;
        assert!(verify(&path, 0, &cancel).is_ok());
        std::fs::write(&path, &bytes[..bytes.len() - 2])?;
        assert!(verify(&path, 0, &cancel).is_err());
        let (sender, cancel) = watch::channel(true);
        let _sender = sender;
        assert!(verify(&path, 0, &cancel).is_err());
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn one_pass_hash_covers_all_compressed_frames_and_reports_input_bytes() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("sandbox-stream-hash-{}.zst", std::process::id()));
        let (_, cancel) = watch::channel(false);
        // A second valid zstd frame must also be included in the file digest.
        let bytes = [compress(&tar_bytes()), compress(&[0; 8192])].concat();
        std::fs::write(&path, &bytes)?;
        let mut progress = Vec::new();
        let digest = verify_with_progress(&path, 0, &cancel, |bytes| progress.push(bytes))?;
        assert_eq!(digest, format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(progress.first(), Some(&0));
        assert_eq!(progress.last(), Some(&(bytes.len() as u64)));
        assert!(progress.windows(2).all(|p| p[0] <= p[1]));
        std::fs::write(&path, &bytes[..bytes.len() - 1])?;
        assert!(verify(&path, 0, &cancel).is_err());
        std::fs::remove_file(path)?;
        Ok(())
    }
}
