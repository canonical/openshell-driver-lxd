// SPDX-License-Identifier: AGPL-3.0-or-later

//! Flattens an OCI image layout into a single rootfs tar stream.
//!
//! `skopeo copy` lays an image out as an OCI image: an `index.json`, a
//! manifest and a stack of (usually gzip-compressed) layer blobs. LXD wants a
//! single rootfs, so the layers must be merged — later layers override earlier
//! ones and the OCI/AUFS whiteout markers delete paths from the layers below.
//! This module performs that merge as a stream and writes the result as a tar
//! for `sqfstar` to turn into the LXD squashfs.
//!
//! Nothing is written to the filesystem and the OCI layout is not modified:
//! the rootfs exists only as the tar stream handed to `sqfstar`, and the OCI
//! blobs are read as they are. File modes (including the setuid/setgid bits),
//! ownership, symbolic links, device nodes and `SCHILY.xattr.*` extended
//! attributes are all carried through into the output tar.
//!
//! Because `sqfstar` builds the squashfs from a tar rather than from files on
//! disk, no `chmod` is ever issued: a strictly confined snap can produce an
//! image whose layers carry setuid binaries, which is why the old
//! unpack-and-repack pipeline — which had to strip those bits first — is no
//! longer needed.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde_json::Value;
use tar::{Archive, Builder, EntryType, Header};

use crate::error::DriverError;

/// Guest path of the injected init script, relative to the rootfs.
const INIT_SCRIPT_NAME: &str = "openshell-init.sh";
/// What the guest's init points at: the injected script, absolute.
const INIT_SCRIPT_GUEST_PATH: &str = "/openshell-init.sh";
/// Linux's symlink-resolution limit, as the kernel's own `MAXSYMLINKS`.
const MAX_SYMLINK_HOPS: usize = 40;

/// A layer blob, in manifest order (bottom-most first).
struct Layer {
    blob: PathBuf,
}

/// An extended attribute, as a raw name/value pair.
type Xattr = (Vec<u8>, Vec<u8>);

/// What [`flatten_image_to_tar`] did, for logging.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FlattenStats {
    /// Entries written to the output tar.
    pub entries: usize,
    /// Extended attributes carried through.
    pub xattrs: usize,
}

/// Reads the layer blobs of the first manifest in an OCI layout, bottom-most
/// first.
fn image_layers(oci_dir: &Path) -> Result<Vec<Layer>, DriverError> {
    let index: Value = read_json(&oci_dir.join("index.json"))?;
    let manifest_digest = index
        .get("manifests")
        .and_then(Value::as_array)
        .and_then(|manifests| manifests.first())
        .and_then(|descriptor| descriptor.get("digest"))
        .and_then(Value::as_str)
        .ok_or_else(|| DriverError::ImageImport("OCI index has no manifests".to_string()))?;

    let manifest: Value = read_json(&blob_path(oci_dir, manifest_digest))?;
    let layers = manifest
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| DriverError::ImageImport("OCI manifest has no layers".to_string()))?;

    layers
        .iter()
        .map(|layer| {
            let digest = layer.get("digest").and_then(Value::as_str).ok_or_else(|| {
                DriverError::ImageImport("OCI layer descriptor has no digest".to_string())
            })?;
            Ok(Layer {
                blob: blob_path(oci_dir, digest),
            })
        })
        .collect()
}

/// The on-disk path of a `sha256:<hex>` blob in an OCI layout.
fn blob_path(oci_dir: &Path, digest: &str) -> PathBuf {
    oci_dir
        .join("blobs")
        .join("sha256")
        .join(digest.strip_prefix("sha256:").unwrap_or(digest))
}

/// Opens a layer blob and wraps it in the decompressor its leading bytes call
/// for.
///
/// The media type in the manifest is advisory — the same image is served as
/// `+gzip` by some registries and `+zstd` by others — so the compression is
/// decided by sniffing the magic bytes instead.
fn open_layer(blob: &Path) -> Result<Box<dyn Read>, DriverError> {
    let mut file = File::open(blob).map_err(|e| {
        DriverError::ImageImport(format!("failed to open layer {}: {e}", blob.display()))
    })?;
    let mut magic = [0u8; 4];
    let mut read = 0;
    while read < magic.len() {
        match file.read(&mut magic[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(DriverError::ImageImport(format!(
                    "failed to read layer {}: {e}",
                    blob.display()
                )))
            }
        }
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| {
        DriverError::ImageImport(format!("failed to rewind layer {}: {e}", blob.display()))
    })?;

    let head = &magic[..read];
    if head.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(GzDecoder::new(file)))
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        // zstd layers are rare and would pull in another decompressor; fail
        // loudly rather than silently truncating the rootfs.
        Err(DriverError::ImageImport(format!(
            "layer {} is zstd-compressed, which this driver does not support",
            blob.display()
        )))
    } else {
        Ok(Box::new(file))
    }
}

/// Iterates the entries that survive the layer merge, top-most first, calling
/// `visit` for each.
///
/// Layers are walked in reverse (top-most first) so the first time a path is
/// seen it is the copy the container would resolve; every later (lower) copy
/// is skipped. Whiteouts hide paths in the layers below the one that carries
/// them, and opaque whiteouts hide everything in a directory below.
fn for_each_merged_entry<F>(layers: &[Layer], mut visit: F) -> Result<(), DriverError>
where
    F: FnMut(&[u8], &mut tar::Entry<'_, Box<dyn Read>>) -> Result<(), DriverError>,
{
    // Paths a whiteout in a layer above has deleted, and directories an opaque
    // whiteout in a layer above has emptied. Both are accumulated as the walk
    // descends, so they only ever apply to layers below the one that declared
    // them.
    let mut deleted: HashSet<Vec<u8>> = HashSet::new();
    let mut opaque: Vec<Vec<u8>> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();

    for layer in layers.iter().rev() {
        let reader = open_layer(&layer.blob)?;
        let mut archive = Archive::new(reader);
        let mut layer_deleted: Vec<Vec<u8>> = Vec::new();
        let mut layer_opaque: Vec<Vec<u8>> = Vec::new();

        let entries = archive
            .entries()
            .map_err(|e| DriverError::ImageImport(format!("failed to read layer tar: {e}")))?;
        for entry in entries {
            let mut entry =
                entry.map_err(|e| DriverError::ImageImport(format!("malformed layer tar: {e}")))?;
            let raw_path = entry.path_bytes().into_owned();
            let Some(path) = normalize_path(&raw_path) else {
                continue;
            };
            let base = basename(&path);

            // OCI whiteout markers are never part of the rootfs.
            if base == b".wh..wh..opq" {
                layer_opaque.push(parent_of(&path).to_vec());
                continue;
            }
            if let Some(stripped) = base.strip_prefix(b".wh.") {
                let mut target = parent_of(&path).to_vec();
                if !target.is_empty() {
                    target.push(b'/');
                }
                target.extend_from_slice(stripped);
                layer_deleted.push(target);
                continue;
            }

            if seen.contains(&path)
                || under_any(deleted.iter(), &path)
                || under_any(opaque.iter(), &path)
            {
                continue;
            }
            seen.insert(path.clone());
            visit(&path, &mut entry)?;
        }

        deleted.extend(layer_deleted);
        opaque.extend(layer_opaque);
    }

    Ok(())
}

/// Flattens the OCI image at `oci_dir` into a single tar written to `out`,
/// injecting `init_script` as the container's init at `/sbin/init`.
///
/// The image's own `/openshell-init.sh` and init path are dropped and replaced
/// with the injected script and a symlink, wherever the image's `sbin` — which
/// a merged-`/usr` image makes a symlink to `usr/sbin` — actually resolves.
pub(crate) fn flatten_image_to_tar<W: Write>(
    oci_dir: &Path,
    init_script: &[u8],
    out: W,
) -> Result<FlattenStats, DriverError> {
    let layers = image_layers(oci_dir)?;
    let init_path = resolve_init_path(&layers)?;

    let mut writer = Builder::new(out);
    let mut stats = FlattenStats::default();

    for_each_merged_entry(&layers, |path, entry| {
        // Drop the image's own init and any copy of the script, at both the
        // plain and merged-`/usr` spellings of the init path. Skipping both
        // means an image never contributes an entry under a path we are about
        // to replace, and keeps a `sbin/init` entry from colliding with a
        // `sbin -> usr/sbin` symlink.
        if path == INIT_SCRIPT_NAME.as_bytes() || path == b"sbin/init" || path == b"usr/sbin/init" {
            return Ok(());
        }
        write_entry(&mut writer, path, entry, &mut stats)
    })?;

    write_bytes(
        &mut writer,
        INIT_SCRIPT_NAME.as_bytes(),
        EntryType::Regular,
        0o755,
        init_script,
        &mut stats,
    )?;
    write_symlink(
        &mut writer,
        &init_path,
        INIT_SCRIPT_GUEST_PATH.as_bytes(),
        &mut stats,
    )?;

    writer
        .into_inner()
        .map_err(|e| DriverError::ImageImport(format!("failed to finish rootfs tar: {e}")))?;
    Ok(stats)
}

/// Extracts the single file `name` (a rootfs-relative path) from the OCI image
/// at `oci_dir`, writing it to `dest` with mode `0755`.
///
/// The effective file — the top-most layer's copy — is what the container
/// would run. A symlink or a non-regular file at that path is rejected, since
/// the driver mounts the file and must be sure it is a regular file.
pub(crate) fn extract_file(oci_dir: &Path, name: &str, dest: &Path) -> Result<(), DriverError> {
    let layers = image_layers(oci_dir)?;
    let wanted = name.as_bytes();
    let mut found = false;

    for_each_merged_entry(&layers, |path, entry| {
        if path != wanted {
            return Ok(());
        }
        found = true;
        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Continuous => {}
            EntryType::Symlink => {
                return Err(DriverError::ImageImport(format!(
                    "image has a symlink at /{name}"
                )))
            }
            _ => {
                return Err(DriverError::ImageImport(format!(
                    "image does not contain a regular file at /{name}"
                )))
            }
        }

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(dest)
            .map_err(|e| {
                DriverError::ImageImport(format!("failed to create {}: {e}", dest.display()))
            })?;
        io::copy(entry, &mut file).map_err(|e| {
            DriverError::ImageImport(format!(
                "failed to extract /{name} to {}: {e}",
                dest.display()
            ))
        })?;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(|e| {
                DriverError::ImageImport(format!("failed to set mode on {}: {e}", dest.display()))
            })?;
        Ok(())
    })?;

    if !found {
        return Err(DriverError::ImageImport(format!(
            "image does not contain /{name}"
        )));
    }
    Ok(())
}

/// Writes one merged entry to the output tar, carrying over its mode,
/// ownership, timestamps, type, link target and extended attributes.
fn write_entry<W: Write, R: Read>(
    writer: &mut Builder<W>,
    path: &[u8],
    entry: &mut tar::Entry<'_, R>,
    stats: &mut FlattenStats,
) -> Result<(), DriverError> {
    let header = entry.header().clone();
    let entry_type = header.entry_type();
    // '7' (contiguous) is a GNU extension sqfstar does not know; it is a
    // regular file in every other respect.
    let out_type = if entry_type == EntryType::Continuous {
        EntryType::Regular
    } else {
        entry_type
    };
    let mode = header.mode().unwrap_or(0o644) & 0o7777;
    let uid = header.uid().unwrap_or(0);
    let gid = header.gid().unwrap_or(0);
    let mtime = header.mtime().unwrap_or(0);
    let size = entry.size();
    let link = entry.link_name_bytes().map(|c| c.into_owned());
    let xattrs = collect_xattrs(entry)?;

    let mut out_header = Header::new_ustar();
    out_header.set_mode(mode);
    out_header.set_uid(uid);
    out_header.set_gid(gid);
    out_header.set_mtime(mtime);
    out_header.set_entry_type(out_type);
    out_header.set_size(size);

    write_xattrs(writer, &xattrs, stats)?;

    let path = OsStr::from_bytes(path);
    match out_type {
        EntryType::Symlink | EntryType::Link => {
            out_header.set_size(0);
            let target = link.unwrap_or_default();
            let target = OsStr::from_bytes(&target);
            writer
                .append_link(&mut out_header, path, target)
                .map_err(|e| DriverError::ImageImport(format!("failed to write tar entry: {e}")))?;
        }
        EntryType::Regular => {
            writer
                .append_data(&mut out_header, path, entry)
                .map_err(|e| DriverError::ImageImport(format!("failed to write tar entry: {e}")))?;
        }
        _ => {
            writer
                .append_data(&mut out_header, path, io::empty())
                .map_err(|e| DriverError::ImageImport(format!("failed to write tar entry: {e}")))?;
        }
    }
    stats.entries += 1;
    Ok(())
}

/// Appends a self-contained regular-file entry whose contents are in memory.
fn write_bytes<W: Write>(
    writer: &mut Builder<W>,
    path: &[u8],
    entry_type: EntryType,
    mode: u32,
    bytes: &[u8],
    stats: &mut FlattenStats,
) -> Result<(), DriverError> {
    let mut header = Header::new_ustar();
    header.set_entry_type(entry_type);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(bytes.len() as u64);
    writer
        .append_data(&mut header, OsStr::from_bytes(path), bytes)
        .map_err(|e| DriverError::ImageImport(format!("failed to write tar entry: {e}")))?;
    stats.entries += 1;
    Ok(())
}

/// Appends a symlink entry at `path` pointing at `target`.
fn write_symlink<W: Write>(
    writer: &mut Builder<W>,
    path: &[u8],
    target: &[u8],
    stats: &mut FlattenStats,
) -> Result<(), DriverError> {
    let mut header = Header::new_ustar();
    header.set_entry_type(EntryType::Symlink);
    header.set_mode(0o777);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(0);
    writer
        .append_link(
            &mut header,
            OsStr::from_bytes(path),
            OsStr::from_bytes(target),
        )
        .map_err(|e| DriverError::ImageImport(format!("failed to write tar symlink: {e}")))?;
    stats.entries += 1;
    Ok(())
}

/// Emits the PAX extended header for a set of xattrs, if any.
fn write_xattrs<W: Write>(
    writer: &mut Builder<W>,
    xattrs: &[Xattr],
    stats: &mut FlattenStats,
) -> Result<(), DriverError> {
    if xattrs.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = xattrs
        .iter()
        .map(|(name, _)| format!("SCHILY.xattr.{}", String::from_utf8_lossy(name)))
        .collect();
    let records: Vec<(&str, &[u8])> = keys
        .iter()
        .zip(xattrs.iter())
        .map(|(key, (_, value))| (key.as_str(), value.as_slice()))
        .collect();
    writer
        .append_pax_extensions(records)
        .map_err(|e| DriverError::ImageImport(format!("failed to write xattr header: {e}")))?;
    stats.xattrs += xattrs.len();
    Ok(())
}

/// Reads the extended attributes `entry` carries in its PAX records, mapping
/// both the `SCHILY` and `LIBARCHIVE` spellings to the `SCHILY` name sqfstar
/// understands.
fn collect_xattrs<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Vec<Xattr>, DriverError> {
    let mut out = Vec::new();
    let extensions = entry
        .pax_extensions()
        .map_err(|e| DriverError::ImageImport(format!("malformed pax header: {e}")))?;
    let Some(extensions) = extensions else {
        return Ok(out);
    };
    for extension in extensions {
        let extension = extension
            .map_err(|e| DriverError::ImageImport(format!("malformed pax record: {e}")))?;
        let key = extension.key_bytes();
        let name = if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
            name
        } else if let Some(name) = key.strip_prefix(b"LIBARCHIVE.xattr.") {
            name
        } else {
            continue;
        };
        out.push((name.to_vec(), extension.value_bytes().to_vec()));
    }
    Ok(out)
}

/// Resolves where the image's `sbin/init` actually lives, following a
/// `sbin -> usr/sbin` (or similar) directory symlink.
fn resolve_init_path(layers: &[Layer]) -> Result<Vec<u8>, DriverError> {
    let mut symlinks: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for_each_merged_entry(layers, |path, entry| {
        if entry.header().entry_type() == EntryType::Symlink {
            if let Some(target) = entry.link_name_bytes() {
                symlinks.insert(path.to_vec(), target.into_owned());
            }
        }
        Ok(())
    })?;

    let dir = resolve_dir(&symlinks, b"sbin");
    let mut init = dir;
    if !init.is_empty() {
        init.push(b'/');
    }
    init.extend_from_slice(b"init");
    Ok(init)
}

/// Resolves a directory path whose components may be symlinks, using only the
/// image's own symlink table.
///
/// Absolute targets restart from the rootfs root and `..` is stopped at it, so
/// a hostile image cannot make the resolution climb out — there is nowhere for
/// it to climb to, the map only knows rootfs-relative paths.
fn resolve_dir(symlinks: &HashMap<Vec<u8>, Vec<u8>>, start: &[u8]) -> Vec<u8> {
    let mut resolved: Vec<u8> = Vec::new();
    let mut pending: Vec<Vec<u8>> = start
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    pending.reverse();
    let mut pending: std::collections::VecDeque<Vec<u8>> = pending.into();
    let mut hops = 0;

    while let Some(component) = pending.pop_front() {
        if component == b"." || component.is_empty() {
            continue;
        }
        if component == b".." {
            resolved = parent_of(&resolved).to_vec();
            continue;
        }
        let mut candidate = resolved.clone();
        if !candidate.is_empty() {
            candidate.push(b'/');
        }
        candidate.extend_from_slice(&component);

        if let Some(target) = symlinks.get(&candidate) {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                break;
            }
            if target.starts_with(b"/") {
                resolved.clear();
            }
            let mut parts: Vec<Vec<u8>> = target
                .split(|&b| b == b'/')
                .filter(|c| !c.is_empty())
                .map(<[u8]>::to_vec)
                .collect();
            parts.reverse();
            for part in parts {
                pending.push_front(part);
            }
        } else {
            resolved = candidate;
        }
    }
    resolved
}

/// Normalizes a tar path to a rootfs-relative form with `/` separators, no
/// leading `./` or `/` and no `.` or `..` components, or `None` when nothing
/// is left.
fn normalize_path(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    for component in raw.split(|&b| b == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            return None;
        }
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(component);
    }
    (!out.is_empty()).then_some(out)
}

/// The last path component.
fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(index) => &path[index + 1..],
        None => path,
    }
}

/// The path with its last component removed, empty at the root.
fn parent_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(index) => &path[..index],
        None => b"",
    }
}

/// True when `path` is one of the entries, or sits beneath one of them.
fn under_any<'a>(dirs: impl Iterator<Item = &'a Vec<u8>>, path: &[u8]) -> bool {
    dirs.into_iter().any(|dir| {
        path == dir.as_slice()
            || (path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/')
    })
}

/// Reads and parses a JSON file.
fn read_json(path: &Path) -> Result<Value, DriverError> {
    let bytes = std::fs::read(path)
        .map_err(|e| DriverError::ImageImport(format!("could not read {}: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| DriverError::ImageImport(format!("invalid JSON in {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    /// Builds one layer tar, gzip-compresses it and writes it as a blob.
    /// Returns the layer descriptor's digest.
    fn write_layer(oci_dir: &Path, entries: &[TestEntry]) -> String {
        let mut tar = Vec::new();
        {
            let mut builder = Builder::new(&mut tar);
            for entry in entries {
                let mut header = Header::new_ustar();
                header.set_entry_type(entry.kind);
                header.set_mode(entry.mode);
                header.set_uid(entry.uid);
                header.set_gid(entry.gid);
                header.set_size(entry.data.len() as u64);
                if let Some(target) = &entry.link {
                    header.set_link_name(OsStr::from_bytes(target)).unwrap();
                }
                let data = entry.data.clone();
                builder
                    .append_data(&mut header, OsStr::from_bytes(&entry.path), &data[..])
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        let compressed = encoder.finish().unwrap();

        let digest = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&compressed);
            format!("sha256:{:x}", hasher.finalize())
        };
        let blobs = oci_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blob_path(oci_dir, &digest), &compressed).unwrap();
        digest
    }

    struct TestEntry {
        path: Vec<u8>,
        kind: EntryType,
        mode: u32,
        uid: u64,
        gid: u64,
        data: Vec<u8>,
        link: Option<Vec<u8>>,
    }

    fn file(path: &str, data: &[u8], mode: u32) -> TestEntry {
        TestEntry {
            path: path.as_bytes().to_vec(),
            kind: EntryType::Regular,
            mode,
            uid: 0,
            gid: 0,
            data: data.to_vec(),
            link: None,
        }
    }

    fn dir(path: &str, mode: u32) -> TestEntry {
        TestEntry {
            path: path.as_bytes().to_vec(),
            kind: EntryType::Directory,
            mode,
            uid: 0,
            gid: 0,
            data: Vec::new(),
            link: None,
        }
    }

    fn symlink(path: &str, target: &str) -> TestEntry {
        TestEntry {
            path: path.as_bytes().to_vec(),
            kind: EntryType::Symlink,
            mode: 0o777,
            uid: 0,
            gid: 0,
            data: Vec::new(),
            link: Some(target.as_bytes().to_vec()),
        }
    }

    /// Writes a minimal OCI layout whose manifest references `layers`,
    /// bottom-most first.
    fn write_layout(dir: &Path, layers: &[String]) {
        let blobs = dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let config = br#"{"rootfs":{"type":"layers","diff_ids":[]}}"#;
        let config_digest = {
            use sha2::{Digest, Sha256};
            format!("sha256:{:x}", Sha256::digest(config))
        };
        std::fs::write(blob_path(dir, &config_digest), config).unwrap();

        let layers_json: Vec<String> = layers
            .iter()
            .map(|d| format!(r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{d}","size":0}}"#))
            .collect();
        let manifest = format!(
            r#"{{"schemaVersion":2,"layers":[{}],"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{}}}}}"#,
            layers_json.join(","),
            config.len()
        );
        let manifest_digest = {
            use sha2::{Digest, Sha256};
            format!("sha256:{:x}", Sha256::digest(manifest.as_bytes()))
        };
        std::fs::write(blob_path(dir, &manifest_digest), manifest.as_bytes()).unwrap();
        let index = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{manifest_digest}","size":{}}}]}}"#,
            manifest.len()
        );
        std::fs::write(dir.join("index.json"), index).unwrap();
    }

    /// Reads a flattened tar back into `(path, mode, type, data)` tuples.
    fn read_tar(bytes: &[u8]) -> Vec<(Vec<u8>, u32, EntryType, Vec<u8>)> {
        let mut archive = Archive::new(bytes);
        let mut out = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path_bytes().into_owned();
            let mode = entry.header().mode().unwrap();
            let kind = entry.header().entry_type();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            out.push((path, mode, kind, data));
        }
        out
    }

    #[test]
    fn later_layers_override_earlier_ones() {
        let temp = tempfile::tempdir().unwrap();
        let layer1 = write_layer(
            temp.path(),
            &[dir("etc", 0o755), file("etc/motd", b"old\n", 0o644)],
        );
        let layer2 = write_layer(temp.path(), &[file("etc/motd", b"new\n", 0o600)]);
        write_layout(temp.path(), &[layer1, layer2]);

        let mut out = Vec::new();
        flatten_image_to_tar(temp.path(), b"#!/bin/sh\n", &mut out).unwrap();
        let entries = read_tar(&out);

        let motd = entries.iter().find(|(p, ..)| p == b"etc/motd").unwrap();
        assert_eq!(motd.3, b"new\n");
        assert_eq!(motd.1, 0o600);
        assert_eq!(entries.iter().filter(|(p, ..)| p == b"etc/motd").count(), 1);
    }

    #[test]
    fn whiteouts_delete_and_opaque_dirs_empty_lower_layers() {
        let temp = tempfile::tempdir().unwrap();
        let layer1 = write_layer(
            temp.path(),
            &[
                dir("a", 0o755),
                file("a/keep", b"1", 0o644),
                file("a/gone", b"1", 0o644),
                file("b", b"1", 0o644),
                file("opq/old", b"1", 0o644),
            ],
        );
        let layer2 = write_layer(
            temp.path(),
            &[
                file("a/.wh.gone", b"", 0o644),
                file("b/.wh..wh..opq", b"", 0o644),
                file("opq/.wh..wh..opq", b"", 0o644),
                file("opq/new", b"2", 0o644),
            ],
        );
        write_layout(temp.path(), &[layer1, layer2]);

        let mut out = Vec::new();
        flatten_image_to_tar(temp.path(), b"x", &mut out).unwrap();
        let paths: Vec<Vec<u8>> = read_tar(&out).into_iter().map(|(p, ..)| p).collect();

        assert!(paths.contains(&b"a/keep".to_vec()));
        assert!(!paths.contains(&b"a/gone".to_vec()), "whiteout deletes");
        assert!(!paths.contains(&b"b".to_vec()), "opaque hides the file");
        assert!(
            !paths.contains(&b"opq/old".to_vec()),
            "opaque hides lower dir contents"
        );
        assert!(
            paths.contains(&b"opq/new".to_vec()),
            "opaque keeps the upper layer"
        );
        assert!(!paths.iter().any(|p| p.starts_with(b".wh.")));
    }

    #[test]
    fn injects_init_and_replaces_the_images_init() {
        let temp = tempfile::tempdir().unwrap();
        let layer = write_layer(
            temp.path(),
            &[
                dir("sbin", 0o755),
                file("sbin/init", b"#!/bin/sh\nexec systemd\n", 0o755),
                file("openshell-init.sh", b"stale", 0o644),
            ],
        );
        write_layout(temp.path(), &[layer]);

        let script = b"#!/bin/sh\necho openshell\n";
        let mut out = Vec::new();
        flatten_image_to_tar(temp.path(), script, &mut out).unwrap();
        let entries = read_tar(&out);

        let init = entries.iter().find(|(p, ..)| p == b"sbin/init").unwrap();
        assert_eq!(init.2, EntryType::Symlink);
        assert_eq!(
            entries.iter().filter(|(p, ..)| p == b"sbin/init").count(),
            1,
            "the image's init is replaced, not duplicated"
        );

        let script_entry = entries
            .iter()
            .find(|(p, ..)| p == b"openshell-init.sh")
            .unwrap();
        assert_eq!(script_entry.3, script);
        assert_eq!(script_entry.1, 0o755);
    }

    #[test]
    fn merged_usr_images_get_their_init_at_usr_sbin() {
        let temp = tempfile::tempdir().unwrap();
        let layer = write_layer(
            temp.path(),
            &[
                dir("usr", 0o755),
                dir("usr/sbin", 0o755),
                symlink("sbin", "usr/sbin"),
                file("usr/sbin/init", b"#!/bin/sh\nexec systemd\n", 0o755),
            ],
        );
        write_layout(temp.path(), &[layer]);

        let mut out = Vec::new();
        flatten_image_to_tar(temp.path(), b"#!/bin/sh\n", &mut out).unwrap();
        let entries = read_tar(&out);

        assert!(
            entries.iter().any(|(p, ..)| p == b"sbin"),
            "the merge symlink survives"
        );
        let init = entries
            .iter()
            .find(|(p, ..)| p == b"usr/sbin/init")
            .unwrap();
        assert_eq!(init.2, EntryType::Symlink);
        assert_eq!(
            entries.iter().filter(|(p, ..)| p == b"sbin/init").count(),
            0,
            "no unreachable sbin/init is emitted"
        );
    }

    #[test]
    fn preserves_setuid_bits_in_modes() {
        let temp = tempfile::tempdir().unwrap();
        let layer = write_layer(
            temp.path(),
            &[
                dir("usr", 0o755),
                dir("usr/bin", 0o755),
                file("usr/bin/passwd", b"x", 0o4755),
                file("usr/bin/chage", b"x", 0o2755),
            ],
        );
        write_layout(temp.path(), &[layer]);

        let mut out = Vec::new();
        flatten_image_to_tar(temp.path(), b"x", &mut out).unwrap();
        let entries = read_tar(&out);

        let passwd = entries
            .iter()
            .find(|(p, ..)| p == b"usr/bin/passwd")
            .unwrap();
        assert_eq!(passwd.1, 0o4755);
        let chage = entries
            .iter()
            .find(|(p, ..)| p == b"usr/bin/chage")
            .unwrap();
        assert_eq!(chage.1, 0o2755);
    }

    #[test]
    fn extract_file_returns_the_top_most_copy() {
        let temp = tempfile::tempdir().unwrap();
        let layer1 = write_layer(temp.path(), &[file("openshell-sandbox", b"old", 0o755)]);
        let layer2 = write_layer(temp.path(), &[file("openshell-sandbox", b"new", 0o755)]);
        write_layout(temp.path(), &[layer1, layer2]);

        let dest = temp.path().join("out.bin");
        extract_file(temp.path(), "openshell-sandbox", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn extract_file_rejects_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let layer = write_layer(temp.path(), &[symlink("openshell-sandbox", "/bin/true")]);
        write_layout(temp.path(), &[layer]);

        let dest = temp.path().join("out.bin");
        let err = extract_file(temp.path(), "openshell-sandbox", &dest).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn normalize_path_rejects_traversal() {
        assert_eq!(normalize_path(b"./a/b"), Some(b"a/b".to_vec()));
        assert_eq!(normalize_path(b"/a//b/"), Some(b"a/b".to_vec()));
        assert_eq!(normalize_path(b"a/../b"), None);
        assert_eq!(normalize_path(b"."), None);
        assert_eq!(normalize_path(b"/"), None);
    }
}
