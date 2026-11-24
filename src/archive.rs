use anyhow::{Context, Result, ensure};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Cursor, Read};
use zip::ZipArchive;

pub const ADDON: &str = "OlympusMytikas";

pub struct SourceFile {
    pub data: Vec<u8>,
    pub git_sha: String,
}

pub type Files = BTreeMap<String, SourceFile>;

pub fn stable_version(version: &str) -> bool {
    let parts: Vec<_> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|s| {
            (1..=8).contains(&s.len())
                && s.bytes().all(|b| b.is_ascii_digit())
                && (s.len() == 1 || !s.starts_with('0'))
        })
}

fn valid_component(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or("");
    !part.is_empty()
        && !part.ends_with('.')
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !matches!(lower.as_str(), ".git" | ".gitattributes" | ".gitmodules")
        && !matches!(stem, "con" | "prn" | "aux" | "nul")
        && !(stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

fn git_blob(data: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(format!("blob {}\0", data.len()));
    hash.update(data);
    format!("{:x}", hash.finalize())
}

fn check_manifest(version: &str, files: &Files) -> Result<()> {
    let toc = &files
        .get("OlympusMytikas.toc")
        .context("Missing OlympusMytikas.toc")?
        .data;
    ensure!(toc.len() <= 1_000_000, "Manifest is too large");
    let toc = std::str::from_utf8(toc)?.trim_start_matches('\u{feff}');
    let mut versions = Vec::new();
    let mut interfaces = Vec::new();
    let mut scripts = HashSet::new();
    for line in toc.lines() {
        if let Some(value) = line.strip_prefix("## Version:") {
            versions.push(value.trim());
        } else if let Some(value) = line.strip_prefix("## Interface:") {
            interfaces.push(value);
        } else if !line.trim().is_empty() && !line.trim_start().starts_with('#') {
            let path = line.trim().replace('\\', "/");
            ensure!(files.contains_key(&path), "Missing manifest file {path}");
            scripts.insert(path);
        }
    }
    ensure!(
        versions == [version],
        "Manifest version must match the website version"
    );
    ensure!(
        interfaces.len() == 1 && interfaces[0].split(',').any(|s| s.trim() == "16001"),
        "Manifest must support Forever interface 16001"
    );
    for script in [
        "Bootstrap.lua",
        "OlympusRecruit.lua",
        "KillOnSight.lua",
        "OlympusMytikas.lua",
    ] {
        ensure!(scripts.contains(script), "Manifest must list {script}");
    }
    Ok(())
}

// Bound the central-directory allocation before the ZIP reader opens untrusted input.
fn check_entry_count(bytes: &[u8]) -> Result<()> {
    let last = bytes.len().checked_sub(22).context("Missing ZIP footer")?;
    let start = last.saturating_sub(u16::MAX as usize);
    let pos = (start..=last)
        .rev()
        .find(|&i| bytes[i..i + 4] == *b"PK\x05\x06")
        .context("Missing ZIP footer")?;
    let footer = &bytes[pos..];
    let comment = u16::from_le_bytes(footer[20..22].try_into()?) as usize;
    ensure!(footer.len() == 22 + comment, "Invalid ZIP footer");
    if let Some(locator) = pos.checked_sub(20).map(|i| &bytes[i..pos])
        && locator.starts_with(b"PK\x06\x07")
    {
        let position = usize::try_from(u64::from_le_bytes(locator[8..16].try_into()?))?;
        let zip64 = bytes
            .get(position..)
            .and_then(|b| b.get(..56))
            .context("Invalid ZIP64 footer")?;
        ensure!(
            zip64.starts_with(b"PK\x06\x06")
                && u64::from_le_bytes(zip64[32..40].try_into()?) <= 2048,
            "Invalid ZIP64 entry count"
        );
    } else {
        let local = u16::from_le_bytes(footer[8..10].try_into()?);
        let total = u16::from_le_bytes(footer[10..12].try_into()?);
        ensure!(local == total && total <= 2048, "Invalid ZIP entry count");
    }
    Ok(())
}

pub fn inspect(bytes: &[u8], version: &str, sha256: &str) -> Result<Files> {
    ensure!(stable_version(version), "Expected stable X.Y.Z version");
    ensure!((1..=8_000_000).contains(&bytes.len()), "Invalid ZIP size");
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == sha256,
        "Stored ZIP checksum mismatch"
    );
    check_entry_count(bytes)?;
    let mut zip = ZipArchive::new(Cursor::new(bytes)).context("Unreadable ZIP")?;
    ensure!(zip.len() <= 2048, "Too many ZIP entries");
    let mut files = Files::new();
    let mut entries = HashSet::new();
    let mut paths = HashMap::new();
    let mut nonempty_dirs = HashSet::new();
    let mut expanded = 0;
    let mut compressed = 0;
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        let name = std::str::from_utf8(entry.name_raw())?.to_owned();
        ensure!(
            name.starts_with("OlympusMytikas/") && name.len() <= 512,
            "Invalid addon path"
        );
        ensure!(entries.insert(name.clone()), "Duplicate ZIP entry");
        let is_dir = name.ends_with('/');
        let central = usize::try_from(entry.central_header_start())?;
        let local = usize::try_from(entry.header_start())?;
        // Validate the two independent descriptions before reading any member bytes.
        let c = bytes
            .get(central..)
            .and_then(|b| b.get(..46))
            .context("Invalid central header")?;
        let l = bytes
            .get(local..)
            .and_then(|b| b.get(..30))
            .context("Invalid local header")?;
        let flags = u16::from_le_bytes([c[8], c[9]]);
        let attributes = u32::from_le_bytes(c[38..42].try_into()?);
        let file_type = (attributes >> 16) & 0o170000;
        ensure!(
            flags & !0x080e == 0
                && l[6..8] == c[8..10]
                && l[8..10] == c[10..12]
                && matches!(u16::from_le_bytes([c[10], c[11]]), 0 | 8),
            "Unsupported ZIP encoding"
        );
        let name_len = u16::from_le_bytes([l[26], l[27]]) as usize;
        ensure!(
            bytes.get(local + 30..local + 30 + name_len) == Some(name.as_bytes()),
            "ZIP headers disagree on file name"
        );
        ensure!(
            attributes & 0x408 == 0
                && (is_dir || attributes & 0x10 == 0)
                && (file_type == 0 || file_type == if is_dir { 0o040000 } else { 0o100000 }),
            "Unsupported ZIP entry type"
        );
        let path = name.trim_end_matches('/');
        let mut prefix = String::new();
        for part in name.strip_suffix('/').unwrap_or(&name).split('/') {
            ensure!(valid_component(part), "Unsupported addon path component");
            if !prefix.is_empty() {
                nonempty_dirs.insert(prefix.clone());
                prefix.push('/');
            }
            prefix.push_str(part);
            let spelling = (prefix.clone(), is_dir || prefix != path);
            let key = prefix.to_ascii_lowercase();
            ensure!(
                paths.entry(key).or_insert_with(|| spelling.clone()) == &spelling,
                "Conflicting addon paths"
            );
            ensure!(paths.len() <= 4096, "Too many addon paths");
        }
        let size = entry.size();
        ensure!(
            size <= 16_000_000 - expanded && (!is_dir || size == 0),
            "ZIP exceeds expanded size limit"
        );
        expanded += size;
        ensure!(
            entry.compressed_size() <= bytes.len() as u64 - compressed,
            "Invalid compressed size"
        );
        compressed += entry.compressed_size();
        let mut data = Vec::new();
        entry.take(size + 1).read_to_end(&mut data)?;
        ensure!(
            data.len() as u64 == size,
            "ZIP size differs from declared size"
        );
        if !is_dir {
            let relative = name.strip_prefix("OlympusMytikas/").unwrap().to_owned();
            files.insert(
                relative,
                SourceFile {
                    git_sha: git_blob(&data),
                    data,
                },
            );
        }
    }
    ensure!(
        paths
            .values()
            .all(|(path, dir)| !dir || nonempty_dirs.contains(path)),
        "Git cannot represent empty directories"
    );
    check_manifest(version, &files)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{ZipWriter, write::FileOptions};

    fn fixture(extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in [
            ("OlympusMytikas/OlympusMytikas.toc", &b"## Interface: 16001\r\n## Version: 1.2.3\r\nBootstrap.lua\r\nOlympusRecruit.lua\r\nKillOnSight.lua\r\nOlympusMytikas.lua\r\n"[..]),
            ("OlympusMytikas/Bootstrap.lua", &b""[..]),
            ("OlympusMytikas/OlympusRecruit.lua", b""),
            ("OlympusMytikas/KillOnSight.lua", b""),
            ("OlympusMytikas/OlympusMytikas.lua", b""),
        ].iter().chain(extra) {
            if name.ends_with('/') {
                writer.add_directory(*name, FileOptions::default()).unwrap();
            } else {
                writer.start_file(*name, FileOptions::default()).unwrap();
                writer.write_all(data).unwrap();
            }
        }
        writer.finish().unwrap().into_inner()
    }

    fn check(bytes: &[u8]) -> Result<Files> {
        inspect(bytes, "1.2.3", &format!("{:x}", Sha256::digest(bytes)))
    }

    #[test]
    fn reads_original_bytes_and_hashes() {
        let bytes = fixture(&[
            ("OlympusMytikas/Media/", b""),
            ("OlympusMytikas/Media/a.tga", &[0, 255]),
        ]);
        let files = check(&bytes).unwrap();
        assert_eq!(files["Media/a.tga"].data, [0, 255]);
        assert_eq!(
            files["Bootstrap.lua"].git_sha,
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert!(inspect(&bytes, "1.2.4", &format!("{:x}", Sha256::digest(&bytes))).is_err());
        assert!(inspect(&bytes, "1.2.3", "bad checksum").is_err());
    }

    #[test]
    fn rejects_oversized_directory_before_entries_are_read() {
        let mut bytes = fixture(&[]);
        let footer = bytes.len() - 22;
        bytes[footer + 8..footer + 12].copy_from_slice(&[255; 4]);
        assert!(check_entry_count(&bytes).is_err());
    }

    #[test]
    fn rejects_out_of_scope_and_unrepresentable_entries() {
        for path in [
            "../file",
            "OlympusMytikas/../file",
            "OlympusMytikas/.git/config",
            "OlympusMytikas/.gitattributes",
            "OlympusMytikas/empty/",
            "OlympusMytikas/bootstrap.lua",
            "OlympusMytikas/Bootstrap.lua",
            "OlympusMytikas/CON.lua",
        ] {
            assert!(check(&fixture(&[(path, b"")])).is_err(), "{path}");
        }
    }
}
