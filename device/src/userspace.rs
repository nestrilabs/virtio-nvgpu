//! Which host files a guest needs in order to drive the forwarded GPU.
//!
//! The guest runs NVIDIA's real user-mode libraries, and those libraries must
//! be the *same build* as the host kernel driver they end up talking to: the
//! ioctls this crate forwards are a private contract between one version of
//! `libcuda` and one version of `nvidia.ko`. Shipping a guest image with its
//! own copy of the driver userspace makes that a coincidence to be maintained,
//! and it breaks the first time a host is updated.
//!
//! So the guest gets the host's copy. This module works out which files that
//! is; a read-only filesystem share carries them across. Nothing is
//! redistributed and the versions cannot drift, because they are the same
//! bytes.
//!
//! The approach is the one container runtimes already use for GPUs -- a
//! container image never contains a driver either -- applied to a VM, where a
//! filesystem share replaces the bind mount.
//!
//! # Where the list comes from
//!
//! Recent drivers ship their own manifest at
//! `/usr/share/nvidia/files.d/sandboxutils-filelist.json`, which is
//! authoritative and versioned with the driver that installed it. Parsing that
//! is strongly preferred to globbing `/usr/lib` for `libnvidia-*`: it
//! distinguishes a library the guest needs from one only the host does, and it
//! says which capability each file serves.
//!
//! # The manifest is not always the loaded driver
//!
//! "versioned with the driver that installed it" is true of the manifest and
//! says nothing about which driver is *running*. A host can carry two driver
//! userspaces at once -- distribution packages for one version and a `.run`
//! installer or a Flatpak runtime for another -- and then the manifest
//! describes whichever install wrote it last, while `libcuda.so.1` points at
//! the other and `nvidia.ko` is the other again.
//!
//! Staging the wrong one is the worst failure this module has, because it does
//! not look like one: the share builds, the guest mounts it, every forwarded
//! ioctl returns 0, and the caller gives up somewhere deep in a driver whose
//! userspace is a different build from the kernel module it is talking to. So
//! [`loaded_driver_version`] reads the version of the module actually loaded
//! and callers compare it against [`staged_driver_version`] before writing a
//! share. Measured on a host with 595.91.07 packages and a 595.99.02 module
//! loaded, where the stager silently picked the packages.
//!
//! # What is deliberately left out
//!
//! Not every entry belongs in a guest, and two kinds actively should not go:
//!
//! - **Firmware** (`gsp_*.bin`, `ucodes_*.bin`) is loaded by the *host* kernel
//!   module onto the GPU. A guest has no GPU to load it onto and no kernel
//!   module to do the loading.
//! - **`nvidia-modprobe`** exists to create `/dev/nvidia*` by loading the real
//!   kernel module. In a guest those nodes are ours, and a setuid helper that
//!   tries to modprobe a driver that is not there can only fail confusingly.
//!
//! Both are filtered by [`Capability`] rather than by name, so a driver that
//! adds a new firmware file does not need a change here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// The driver's own manifest, on a host where one is installed.
pub const DEFAULT_MANIFEST: &str = "/usr/share/nvidia/files.d/sandboxutils-filelist.json";

/// Where the running kernel module reports its own version.
pub const LOADED_VERSION_PATH: &str = "/proc/driver/nvidia/version";

/// The version of the NVIDIA kernel module currently loaded, from the text
/// `/proc/driver/nvidia/version` exposes:
///
/// ```text
/// NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.99.02  Wed Aug 26 ...
/// ```
///
/// `None` when the file is absent (no driver loaded) or holds nothing
/// version-shaped. The caller decides what that means: it is normal when
/// planning a share on a machine with no GPU, and a reason to stop when about
/// to stage one for a guest.
pub fn loaded_driver_version(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    // Deliberately not anchored to "NVRM version:" -- the line has been
    // reworded across releases, while the dotted version on it has not.
    text.lines().find_map(version_in)
}

/// The driver version the resolved files belong to, taken from the library
/// names themselves (`libcuda.so.595.91.07`) rather than from the manifest,
/// which does not state one.
///
/// Files are named individually, so this returns the version the *most* of
/// them carry: a stray file from another install should not decide the answer
/// for the share as a whole.
pub fn staged_driver_version(found: &[Resolved]) -> Option<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in found {
        if let Some(v) = r.host_path.file_name().and_then(|n| version_in(&n.to_string_lossy())) {
            *counts.entry(v).or_default() += 1;
        }
    }
    // Ties broken by the higher version, so the answer is stable rather than
    // whichever the map happened to yield first.
    counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0))).map(|(v, _)| v)
}

/// The first `N.N.N` (or longer) dotted number in `s`, where every component
/// is digits. Written out rather than pulled from a regex crate: this crate
/// carries no such dependency, and the shape is fixed.
fn version_in(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut dots = 0;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            if bytes[i] == b'.' {
                // "595.99.02." or "1..2" is not a version; stop before the dot.
                if i + 1 >= bytes.len() || !bytes[i + 1].is_ascii_digit() {
                    break;
                }
                dots += 1;
            }
            i += 1;
        }
        if dots >= 2 {
            return Some(s[start..i].to_string());
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    None
}

/// Directories a manifest entry's bare name is resolved against, in order.
///
/// The manifest gives basenames, not paths -- `libcuda.so.615.71.09`, not
/// `/usr/lib/libcuda.so.615.71.09` -- because where a distribution puts them is
/// the distribution's business. Debian multiarch, Arch and the `.run` installer
/// all differ.
pub const DEFAULT_SEARCH_PATHS: &[&str] = &[
    "/usr/lib",
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/bin",
    "/usr/share/vulkan/icd.d",
    "/usr/share/vulkan/implicit_layer.d",
    "/usr/share/vulkansc/icd.d",
    "/usr/share/glvnd/egl_vendor.d",
    "/usr/share/nvidia",
    "/usr/lib/nvidia",
    "/usr/lib/xorg/modules/drivers",
    "/usr/lib/xorg/modules/extensions",
];

/// What a file is, as the driver's manifest classifies it.
///
/// Unknown values are kept rather than rejected: a newer driver introducing a
/// type this was not written against should degrade to "carry it if its
/// category matches", not abort.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileKind {
    Lib,
    Binary,
    Json,
    Firmware,
    Symlink,
    Modprobe,
    Other(String),
}

impl FileKind {
    fn parse(s: &str) -> Self {
        match s {
            "LIB" => Self::Lib,
            "BINARY" => Self::Binary,
            "JSON" | "ICD" => Self::Json,
            "FIRMWARE" => Self::Firmware,
            "SYMLINK" => Self::Symlink,
            "MODPROBE" => Self::Modprobe,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lib => write!(f, "lib"),
            Self::Binary => write!(f, "binary"),
            Self::Json => write!(f, "json"),
            Self::Firmware => write!(f, "firmware"),
            Self::Symlink => write!(f, "symlink"),
            Self::Modprobe => write!(f, "modprobe"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

/// A coarse capability, in the sense container runtimes use the word: a name
/// for a set of the driver's own finer-grained categories.
///
/// These exist so a caller can ask for "compute" without tracking that the
/// driver spells it `cuda` in some entries and `opencl` in others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// `nvidia-smi` and NVML -- the smallest useful set, and enough to tell
    /// whether forwarding works at all.
    Utility,
    /// CUDA, OpenCL and the PTX JIT.
    Compute,
    /// Vulkan, EGL and GL, including the headless EGL a guest compositor uses.
    Graphics,
    /// NVENC and NVDEC.
    Video,
}

impl Capability {
    /// The driver's category names this capability covers.
    fn categories(self) -> &'static [&'static str] {
        match self {
            Self::Utility => &["nvml", "utils"],
            Self::Compute => &["cuda", "opencl", "optix"],
            Self::Graphics => &[
                "vulkan",
                "egl",
                "egl_headless",
                "egl_wayland",
                "egl_gbm",
                "egl_x11",
                "glx",
                "gbm",
                "glvnd",
            ],
            Self::Video => &["video"],
        }
    }

    /// What a headless render-and-encode guest needs: everything but the
    /// host-only categories. This is the set the streaming pipeline uses.
    pub fn headless_pipeline() -> &'static [Capability] {
        &[
            Capability::Utility,
            Capability::Compute,
            Capability::Graphics,
            Capability::Video,
        ]
    }
}

/// One file the driver declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// As the manifest gives it: usually a bare filename, occasionally with a
    /// leading component such as `firmware/`.
    pub name: String,
    pub kind: FileKind,
    pub categories: BTreeSet<String>,
}

impl Entry {
    /// Whether any of `caps` covers this entry.
    pub fn wanted_by(&self, caps: &[Capability]) -> bool {
        caps.iter()
            .flat_map(|c| c.categories())
            .any(|c| self.categories.contains(*c))
    }

    /// Whether this entry must never be carried into a guest, whatever
    /// capability asked for it. See the module docs.
    pub fn is_host_only(&self) -> bool {
        matches!(self.kind, FileKind::Firmware | FileKind::Modprobe)
            || self.categories.contains("firmware")
            || self.categories.contains("modprobe")
    }
}

/// A resolved entry: a manifest line plus where it actually is on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub entry: Entry,
    pub host_path: PathBuf,
    /// Where it should land in the guest, relative to the share root.
    pub guest_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum UserspaceError {
    #[error("no driver manifest at {0} -- this host's driver predates one, or is not installed")]
    NoManifest(PathBuf),
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not the array of objects a driver manifest is: {detail}")]
    Malformed { path: PathBuf, detail: String },
}

/// Parse a driver manifest.
///
/// Hand-written rather than pulling in a JSON crate: the shape is a flat array
/// of objects whose values are strings or arrays of strings, this crate carries
/// no serialization dependency, and a parser that accepts exactly that shape
/// rejects a malformed file more usefully than a general one would.
pub fn parse_manifest(src: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;

    skip_ws(b, &mut i);
    if b.get(i) != Some(&b'[') {
        return Err("expected a top-level array".into());
    }
    i += 1;

    loop {
        skip_ws(b, &mut i);
        match b.get(i) {
            Some(b']') => break,
            Some(b',') => {
                i += 1;
                continue;
            }
            Some(b'{') => {}
            Some(c) => return Err(format!("expected an object, found {:?}", *c as char)),
            None => return Err("file ended inside the array".into()),
        }
        i += 1;

        let mut name = None;
        let mut kind = None;
        let mut categories = BTreeSet::new();

        loop {
            skip_ws(b, &mut i);
            match b.get(i) {
                Some(b'}') => {
                    i += 1;
                    break;
                }
                Some(b',') => {
                    i += 1;
                    continue;
                }
                Some(b'"') => {}
                _ => return Err("expected a key".into()),
            }
            let key = read_string(b, &mut i)?;
            skip_ws(b, &mut i);
            if b.get(i) != Some(&b':') {
                return Err(format!("no ':' after key {key:?}"));
            }
            i += 1;
            skip_ws(b, &mut i);

            match b.get(i) {
                Some(b'"') => {
                    let v = read_string(b, &mut i)?;
                    match key.as_str() {
                        "name" => name = Some(v),
                        "type" => kind = Some(FileKind::parse(&v)),
                        _ => {}
                    }
                }
                Some(b'[') => {
                    i += 1;
                    loop {
                        skip_ws(b, &mut i);
                        match b.get(i) {
                            Some(b']') => {
                                i += 1;
                                break;
                            }
                            Some(b',') => {
                                i += 1;
                            }
                            Some(b'"') => {
                                let v = read_string(b, &mut i)?;
                                if key == "category" {
                                    categories.insert(v);
                                }
                            }
                            _ => return Err("expected a string in an array".into()),
                        }
                    }
                }
                // A value this parser does not model -- a number, bool or null.
                // Skipped rather than rejected: an unknown field is not a
                // reason to refuse a manifest whose entries are otherwise fine.
                _ => skip_scalar(b, &mut i),
            }
        }

        let name = name.ok_or("an entry has no \"name\"")?;
        entries.push(Entry {
            name,
            kind: kind.unwrap_or(FileKind::Other(String::new())),
            categories,
        });
    }

    Ok(entries)
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while matches!(b.get(*i), Some(c) if c.is_ascii_whitespace()) {
        *i += 1;
    }
}

fn skip_scalar(b: &[u8], i: &mut usize) {
    while !matches!(b.get(*i), None | Some(b',') | Some(b'}') | Some(b']')) {
        *i += 1;
    }
}

fn read_string(b: &[u8], i: &mut usize) -> Result<String, String> {
    if b.get(*i) != Some(&b'"') {
        return Err("expected a string".into());
    }
    *i += 1;
    let mut out = String::new();
    loop {
        match b.get(*i) {
            None => return Err("file ended inside a string".into()),
            Some(b'"') => {
                *i += 1;
                return Ok(out);
            }
            Some(b'\\') => {
                *i += 1;
                match b.get(*i) {
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(c) => out.push(*c as char),
                    None => return Err("file ended inside an escape".into()),
                }
                *i += 1;
            }
            Some(c) => {
                out.push(*c as char);
                *i += 1;
            }
        }
    }
}

/// Rewrite a manifest's entries to name a different driver build.
///
/// The manifest conflates two things this module needs to keep apart: *which*
/// files a guest needs, with what capability each serves, and *which build* of
/// them this host has. Only the first is hard to work out, and only the second
/// goes stale. A host carrying two driver userspaces has a manifest that is
/// still right about the first and wrong about the second.
///
/// So keep the categories and swap the version. NVIDIA versions its payload
/// files by filename suffix (`libcuda.so.595.99.02`), which is the contract
/// that makes this sound: an entry naming `from` has a `to` counterpart iff
/// that build is installed, and [`resolve`] will simply not find the ones that
/// are not.
///
/// Entries with no version in their name -- `nvidia_icd.json`, `nvidia-smi` --
/// are returned unchanged, which is correct: they are not versioned on disk.
pub fn retarget(entries: &[Entry], from: &str, to: &str) -> Vec<Entry> {
    entries
        .iter()
        .map(|e| Entry {
            name: e.name.replace(from, to),
            kind: e.kind.clone(),
            categories: e.categories.clone(),
        })
        .collect()
}

/// Read the manifest this host's driver installed.
pub fn load_manifest(path: &Path) -> Result<Vec<Entry>, UserspaceError> {
    if !path.exists() {
        return Err(UserspaceError::NoManifest(path.to_path_buf()));
    }
    let src = std::fs::read_to_string(path).map_err(|source| UserspaceError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_manifest(&src).map_err(|detail| UserspaceError::Malformed {
        path: path.to_path_buf(),
        detail,
    })
}

/// Where a file of this kind belongs in the guest, relative to the share root.
///
/// The guest layout deliberately mirrors a normal filesystem rather than being
/// flat, so the share can be mounted and its `lib` directory handed to
/// `ldconfig` and its `share` directory to the Vulkan loader, with no rewriting
/// of the JSON manifests that point at library names.
fn guest_dir(entry: &Entry, host_path: &Path) -> PathBuf {
    match entry.kind {
        FileKind::Binary | FileKind::Modprobe => PathBuf::from("bin"),
        FileKind::Json => {
            // Keep a JSON file under the same tail as the host has it, so the
            // loader finds it where it expects: icd.d, implicit_layer.d and
            // egl_vendor.d are not interchangeable, and the loader scans each
            // by name.
            //
            // Anchored on the `share` component rather than on a `/usr`
            // prefix, so /usr/share, /usr/local/share and a staging root all
            // yield the same tail. A host that keeps its manifests somewhere
            // with no `share` at all still gets a sound answer: the directory
            // the loader keys off is the immediate parent, so that name is
            // preserved under `share`.
            let parent = host_path.parent().unwrap_or(Path::new(""));
            let mut comps = parent.components();
            if comps.any(|c| c.as_os_str() == "share") {
                Path::new("share").join(comps.as_path())
            } else {
                match parent.file_name() {
                    Some(tail) => Path::new("share").join(tail),
                    None => PathBuf::from("share"),
                }
            }
        }
        _ => PathBuf::from("lib"),
    }
}

/// Find each wanted entry on this host.
///
/// Returns what was found and what was not, each deduplicated.
///
/// Deduplication is not defensive tidying. A real driver manifest lists the
/// same file once per group of capabilities it belongs to, so a host's 110
/// entries yield 80 selections covering 46 distinct files. Left alone, the
/// second attempt to stage a path fails with `EEXIST` and the share is built
/// only as far as the first duplicate.
///
/// A missing file is reported rather than being an error: manifests cover
/// optional packages, and a guest that only needs compute should not be
/// blocked by an absent Wayland EGL library that was never installed.
pub fn resolve(
    entries: &[Entry],
    caps: &[Capability],
    search_paths: &[impl AsRef<Path>],
) -> (Vec<Resolved>, Vec<Entry>) {
    let mut found: Vec<Resolved> = Vec::new();
    let mut missing: Vec<Entry> = Vec::new();
    let mut staged = BTreeSet::new();
    let mut absent = BTreeSet::new();

    for entry in entries {
        if entry.is_host_only() || !entry.wanted_by(caps) {
            continue;
        }
        // The manifest may carry a leading directory component; only the file
        // name is searched for, and the component is not reused as a path.
        let base = Path::new(&entry.name)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());

        match search_paths
            .iter()
            .map(|d| d.as_ref().join(&base))
            .find(|p| p.exists())
        {
            Some(host_path) => {
                let guest_path = guest_dir(entry, &host_path).join(&base);
                // Merge the categories of a repeated entry into the one that
                // is kept, so the record says every capability the file serves.
                if let Some(prev) = found.iter_mut().find(|r| r.guest_path == guest_path) {
                    prev.entry.categories.extend(entry.categories.iter().cloned());
                    continue;
                }
                if staged.insert(guest_path.clone()) {
                    found.push(Resolved {
                        entry: entry.clone(),
                        host_path,
                        guest_path,
                    });
                }
            }
            None => {
                if absent.insert(entry.name.clone()) {
                    missing.push(entry.clone());
                }
            }
        }
    }

    (found, missing)
}

/// The `SONAME` a shared library records for itself, if it has one.
///
/// A driver manifest names the real files -- `libnvidia-ml.so.615.71.09` --
/// and not the `libnvidia-ml.so.1` symlink a caller actually dlopens, because
/// that link is made by packaging rather than shipped. A share built from the
/// manifest alone therefore contains every library and resolves none of them,
/// which surfaces as "couldn't find libnvidia-ml.so" from a tool that is
/// staring right at it.
///
/// The major version cannot be guessed: across one driver it is `.so.1` for
/// libcuda, `.so.0` for libGLX_nvidia and `.so.4` for libnvidia-nvvm70. So it
/// is read from the ELF, which is where the loader reads it from too.
///
/// Returns `None` for anything that is not an ELF64 little-endian shared
/// object with a `DT_SONAME`, which includes every non-library in the
/// manifest. That is not an error: a binary or a JSON file simply has none.
pub fn soname(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: i64 = 0;
    const DT_STRTAB: i64 = 5;
    const DT_SONAME: i64 = 14;

    let mut f = std::fs::File::open(path).ok()?;

    let mut ehdr = [0u8; 64];
    f.read_exact(&mut ehdr).ok()?;
    // Only ELF64 little-endian is in scope; this project is x86_64 host and
    // guest, and a wrong-width parse would read garbage rather than fail.
    if &ehdr[0..4] != b"\x7fELF" || ehdr[4] != 2 || ehdr[5] != 1 {
        return None;
    }
    // Infallible by construction: every caller below indexes into a buffer it
    // has already read to an exact, known size.
    let u16at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32at = |b: &[u8], o: usize| {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    };
    let u64at = |b: &[u8], o: usize| {
        u64::from_le_bytes([
            b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
        ])
    };

    let phoff = u64at(&ehdr, 0x20);
    let phentsize = u16at(&ehdr, 0x36) as usize;
    let phnum = u16at(&ehdr, 0x38) as usize;
    if phentsize < 56 || phnum == 0 {
        return None;
    }

    let mut phdrs = vec![0u8; phentsize * phnum];
    f.seek(SeekFrom::Start(phoff)).ok()?;
    f.read_exact(&mut phdrs).ok()?;

    // DT_STRTAB is a virtual address, so it has to be translated back to a file
    // offset through the PT_LOAD segments. Skipping this and treating it as an
    // offset happens to work for some libraries and silently reads the wrong
    // bytes for others.
    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..phnum {
        let p = &phdrs[i * phentsize..][..56];
        let p_type = u32at(p, 0);
        let p_offset = u64at(p, 8);
        let p_vaddr = u64at(p, 16);
        let p_filesz = u64at(p, 32);
        match p_type {
            PT_LOAD => loads.push((p_vaddr, p_offset, p_filesz)),
            PT_DYNAMIC => dynamic = Some((p_offset, p_filesz)),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic?;
    let vaddr_to_off = |v: u64| -> Option<u64> {
        loads
            .iter()
            .find(|(va, _, sz)| v >= *va && v < va + sz)
            .map(|(va, off, _)| off + (v - va))
    };

    let mut dynbuf = vec![0u8; dyn_size as usize];
    f.seek(SeekFrom::Start(dyn_off)).ok()?;
    f.read_exact(&mut dynbuf).ok()?;

    let mut soname_off = None;
    let mut strtab = None;
    for e in dynbuf.chunks_exact(16) {
        let tag = i64::from_le_bytes(e[0..8].try_into().expect("16-byte chunk"));
        let val = u64::from_le_bytes(e[8..16].try_into().expect("16-byte chunk"));
        match tag {
            DT_NULL => break,
            DT_SONAME => soname_off = Some(val),
            DT_STRTAB => strtab = Some(val),
            _ => {}
        }
    }

    let strtab_file = vaddr_to_off(strtab?)?;
    let mut name = Vec::new();
    f.seek(SeekFrom::Start(strtab_file + soname_off?)).ok()?;
    // A SONAME is short; read a bounded window and stop at the NUL rather than
    // trusting a length from the file.
    let mut window = [0u8; 256];
    let n = f.read(&mut window).ok()?;
    for &b in &window[..n] {
        if b == 0 {
            break;
        }
        name.push(b);
    }
    if name.is_empty() {
        return None;
    }
    String::from_utf8(name).ok()
}

#[cfg(test)]
mod tests {
    use super::{loaded_driver_version, staged_driver_version, version_in, Entry, FileKind, Resolved};
    use std::collections::BTreeSet as TestSet;

    fn resolved(host_name: &str) -> Resolved {
        Resolved {
            entry: Entry {
                name: host_name.to_string(),
                kind: FileKind::Lib,
                categories: TestSet::new(),
            },
            host_path: std::path::PathBuf::from("/usr/lib/x86_64-linux-gnu").join(host_name),
            guest_path: std::path::PathBuf::from("lib").join(host_name),
        }
    }

    #[test]
    fn retargeting_swaps_the_build_and_keeps_the_categories() {
        let mut cats = TestSet::new();
        cats.insert("compute".to_string());
        let entries = vec![
            Entry { name: "libcuda.so.595.91.07".into(), kind: FileKind::Lib, categories: cats.clone() },
            // Unversioned on disk, so it must survive untouched.
            Entry { name: "nvidia_icd.json".into(), kind: FileKind::Json, categories: cats.clone() },
        ];
        let out = super::retarget(&entries, "595.91.07", "595.99.02");
        assert_eq!(out[0].name, "libcuda.so.595.99.02");
        assert_eq!(out[0].categories, cats, "capability must survive the swap");
        assert_eq!(out[1].name, "nvidia_icd.json", "an unversioned name is not rewritten");
    }

    #[test]
    fn version_is_read_from_the_proc_line_the_module_writes() {
        let dir = std::env::temp_dir().join(format!("nvgpu-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("version");
        std::fs::write(
            &p,
            "NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.99.02  Wed Aug 26 05:24:08 UTC 2026\n\
             GCC version:  gcc version 15.2.0 (Ubuntu 15.2.0-16ubuntu1)\n",
        )
        .unwrap();
        assert_eq!(loaded_driver_version(&p).as_deref(), Some("595.99.02"));
        // A host with no driver loaded has no such file, and that is not an
        // error here -- planning a share off-box is a legitimate use.
        assert_eq!(loaded_driver_version(&dir.join("absent")), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_gcc_version_on_the_second_line_is_not_mistaken_for_the_driver() {
        // "gcc version 15.2.0" is dotted and comes second; the driver version
        // is on the first line, so a find_map over lines must take that one.
        assert_eq!(version_in("GCC version:  gcc version 15.2.0 (Ubuntu)"), Some("15.2.0".into()));
        assert_eq!(version_in("libcuda.so.1"), None, "so.1 is not a driver version");
        assert_eq!(version_in("libcuda.so.595.99.02"), Some("595.99.02".into()));
        assert_eq!(version_in("no digits here"), None);
    }

    #[test]
    fn the_staged_version_is_the_one_most_files_carry() {
        // The shape that caused this check to exist: a host carrying two
        // driver userspaces, where the manifest described the packaged one.
        let found = vec![
            resolved("libcuda.so.595.91.07"),
            resolved("libnvidia-glcore.so.595.91.07"),
            resolved("libnvidia-eglcore.so.595.91.07"),
            resolved("libnvidia-sandboxutils.so.595.99.02"),
            resolved("libnvidia-egl-wayland.so.1.1.21"),
        ];
        assert_eq!(staged_driver_version(&found).as_deref(), Some("595.91.07"));
        assert_eq!(staged_driver_version(&[]), None);
    }

    use super::*;

    /// A sample in the driver's format, written here rather than copied from a
    /// driver package. It carries one of every case the resolver has to get
    /// right, including the two host-only kinds.
    const SAMPLE: &str = r#"[
        {"name": "firmware/gsp_ga10x.bin", "type": "FIRMWARE", "category": ["firmware"]},
        {"name": "nvidia-modprobe", "type": "MODPROBE", "category": ["modprobe"]},
        {"name": "nvidia-smi", "type": "BINARY", "category": ["nvml"]},
        {"name": "libnvidia-ml.so.615.71.09", "type": "LIB", "category": ["nvml"]},
        {"name": "libcuda.so.615.71.09", "type": "LIB", "category": ["cuda", "optix", "vulkan"]},
        {"name": "libnvidia-encode.so.615.71.09", "type": "LIB", "category": ["video"]},
        {"name": "libnvidia-eglcore.so.615.71.09", "type": "LIB", "category": ["egl", "egl_headless"]},
        {"name": "nvidia_icd.json", "type": "JSON", "category": ["vulkan"]},
        {"name": "libnvidia-fbc.so.615.71.09", "type": "LIB", "category": ["xdriver"], "optional": true}
    ]"#;

    fn sample() -> Vec<Entry> {
        parse_manifest(SAMPLE).expect("the sample parses")
    }

    #[test]
    fn parses_every_entry_with_its_kind_and_categories() {
        let e = sample();
        assert_eq!(e.len(), 9);
        assert_eq!(e[2].name, "nvidia-smi");
        assert_eq!(e[2].kind, FileKind::Binary);
        assert!(e[4].categories.contains("cuda"));
        assert!(e[4].categories.contains("vulkan"));
    }

    /// The `optional` key is a bool this parser does not model. An unknown
    /// field must not cost us the entry it appears on.
    #[test]
    fn an_unmodelled_field_does_not_lose_the_entry() {
        let e = sample();
        let last = e.last().expect("a last entry");
        assert_eq!(last.name, "libnvidia-fbc.so.615.71.09");
        assert_eq!(last.kind, FileKind::Lib);
    }

    #[test]
    fn firmware_and_modprobe_are_host_only() {
        let e = sample();
        assert!(e[0].is_host_only(), "GSP firmware must not reach a guest");
        assert!(e[1].is_host_only(), "nvidia-modprobe must not reach a guest");
        assert!(!e[2].is_host_only());
    }

    #[test]
    fn capabilities_select_disjoint_sets() {
        let e = sample();
        let names = |caps: &[Capability]| -> Vec<&str> {
            e.iter()
                .filter(|x| !x.is_host_only() && x.wanted_by(caps))
                .map(|x| x.name.as_str())
                .collect()
        };
        assert_eq!(names(&[Capability::Utility]), ["nvidia-smi", "libnvidia-ml.so.615.71.09"]);
        assert_eq!(names(&[Capability::Video]), ["libnvidia-encode.so.615.71.09"]);
        assert!(names(&[Capability::Compute]).contains(&"libcuda.so.615.71.09"));
    }

    /// `libcuda` is categorised for cuda *and* vulkan. Asking for both must not
    /// stage it twice, or the share gets a duplicate and `ldconfig` a warning.
    #[test]
    fn a_file_in_two_categories_resolves_once() {
        let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
        let (found, _) = resolve(
            &sample(),
            &[Capability::Compute, Capability::Graphics],
            &[dir.path().join("lib")],
        );
        let cuda: Vec<_> = found
            .iter()
            .filter(|r| r.entry.name.starts_with("libcuda"))
            .collect();
        assert_eq!(cuda.len(), 1, "libcuda was staged {} times", cuda.len());
    }

    #[test]
    fn a_json_keeps_the_directory_tail_its_loader_looks_in() {
        let dir = TempTree::new(&["share/vulkan/icd.d/nvidia_icd.json"]);
        let (found, _) = resolve(
            &sample(),
            &[Capability::Graphics],
            &[dir.path().join("share/vulkan/icd.d")],
        );
        let icd = found
            .iter()
            .find(|r| r.entry.name == "nvidia_icd.json")
            .expect("the ICD resolved");
        assert!(
            icd.guest_path.ends_with("vulkan/icd.d/nvidia_icd.json"),
            "an ICD landed at {:?}, where the Vulkan loader will not look",
            icd.guest_path
        );
    }

    /// How a real manifest repeats itself: the same file appears once per
    /// group of capabilities it serves, as separate entries rather than one
    /// entry with several categories. On a real host this turned 110 entries
    /// into 80 selections over 46 distinct files.
    const SAMPLE_WITH_REPEATS: &str = r#"[
        {"name": "libcuda.so.615.71.09", "type": "LIB", "category": ["cuda"]},
        {"name": "libcuda.so.615.71.09", "type": "LIB", "category": ["vulkan"]},
        {"name": "libnvidia-opencl.so.615.71.09", "type": "LIB", "category": ["opencl"]},
        {"name": "libnvidia-opencl.so.615.71.09", "type": "LIB", "category": ["cuda"]}
    ]"#;

    /// Staging the same path twice fails with EEXIST and leaves the share
    /// built only as far as the first duplicate, so a repeated entry must
    /// resolve once.
    #[test]
    fn a_file_listed_twice_is_staged_once() {
        let entries = parse_manifest(SAMPLE_WITH_REPEATS).expect("parses");
        assert_eq!(entries.len(), 4, "the manifest really does repeat entries");

        let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
        let (found, missing) = resolve(
            &entries,
            &[Capability::Compute, Capability::Graphics],
            &[dir.path().join("lib")],
        );

        assert_eq!(found.len(), 1, "libcuda staged {} times", found.len());
        assert_eq!(missing.len(), 1, "the absent file was reported {} times", missing.len());
    }

    /// The categories of a repeated entry belong to the one that is kept --
    /// otherwise the record says libcuda serves compute but not graphics,
    /// purely because of which duplicate came first.
    #[test]
    fn a_repeated_entry_keeps_every_category_it_was_listed_under() {
        let entries = parse_manifest(SAMPLE_WITH_REPEATS).expect("parses");
        let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
        let (found, _) = resolve(
            &entries,
            &[Capability::Compute, Capability::Graphics],
            &[dir.path().join("lib")],
        );
        let cuda = &found[0].entry;
        assert!(cuda.categories.contains("cuda"));
        assert!(cuda.categories.contains("vulkan"), "categories were {:?}", cuda.categories);
    }

    #[test]
    fn what_is_absent_is_reported_rather_than_failing() {
        let dir = TempTree::new(&["lib/libcuda.so.615.71.09"]);
        let (found, missing) = resolve(
            &sample(),
            &[Capability::Compute, Capability::Video],
            &[dir.path().join("lib")],
        );
        assert_eq!(found.len(), 1);
        assert!(
            missing.iter().any(|e| e.name.starts_with("libnvidia-encode")),
            "an absent file should be reported, not silently dropped"
        );
    }

    /// A minimal but structurally valid ELF64 shared object carrying one
    /// SONAME. Built here rather than read from the system so the test says
    /// the same thing on a machine with no NVIDIA driver, and so the PT_LOAD
    /// below can give vaddr and file offset *different* values -- treating
    /// DT_STRTAB as a file offset works by accident when they coincide, which
    /// is exactly the bug this guards.
    fn synthetic_so(soname: &str) -> Vec<u8> {
        const VADDR_BASE: u64 = 0x1000;
        let ehdr_len = 64usize;
        let phdr_len = 56usize;
        let dyn_off = (ehdr_len + phdr_len * 2) as u64;
        let dyn_len = 48u64; // three 16-byte entries
        let strtab_off = dyn_off + dyn_len;
        // index 0 is the customary empty string
        let strtab = {
            let mut v = vec![0u8];
            v.extend_from_slice(soname.as_bytes());
            v.push(0);
            v
        };

        let mut f = vec![0u8; (strtab_off as usize) + strtab.len()];
        f[0..4].copy_from_slice(b"\x7fELF");
        f[4] = 2; // ELF64
        f[5] = 1; // little endian
        f[6] = 1; // version
        f[0x20..0x28].copy_from_slice(&(ehdr_len as u64).to_le_bytes()); // e_phoff
        f[0x36..0x38].copy_from_slice(&(phdr_len as u16).to_le_bytes()); // e_phentsize
        f[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

        let total = f.len() as u64;
        // PT_LOAD covering the whole file, mapped at VADDR_BASE.
        let p0 = ehdr_len;
        f[p0..p0 + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        f[p0 + 8..p0 + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
        f[p0 + 16..p0 + 24].copy_from_slice(&VADDR_BASE.to_le_bytes()); // p_vaddr
        f[p0 + 32..p0 + 40].copy_from_slice(&total.to_le_bytes()); // p_filesz

        // PT_DYNAMIC
        let p1 = ehdr_len + phdr_len;
        f[p1..p1 + 4].copy_from_slice(&2u32.to_le_bytes()); // PT_DYNAMIC
        f[p1 + 8..p1 + 16].copy_from_slice(&dyn_off.to_le_bytes());
        f[p1 + 32..p1 + 40].copy_from_slice(&dyn_len.to_le_bytes());

        let mut put = |i: usize, tag: i64, val: u64| {
            let o = dyn_off as usize + i * 16;
            f[o..o + 8].copy_from_slice(&tag.to_le_bytes());
            f[o + 8..o + 16].copy_from_slice(&val.to_le_bytes());
        };
        put(0, 14, 1); // DT_SONAME -> strtab index 1
        put(1, 5, VADDR_BASE + strtab_off); // DT_STRTAB, as a vaddr
        put(2, 0, 0); // DT_NULL

        f[strtab_off as usize..].copy_from_slice(&strtab);
        f
    }

    #[test]
    fn soname_is_read_from_the_elf_not_guessed_from_the_filename() {
        let dir = TempTree::new(&[]);
        let path = dir.path().join("libnvidia-nvvm70.so.615.71.09");
        std::fs::create_dir_all(dir.path()).expect("mkdir");
        std::fs::write(&path, synthetic_so("libnvidia-nvvm70.so.4")).expect("write");

        // The filename says 615.71.09 and the SONAME says 4. Any scheme that
        // derives the link from the filename gets this wrong, and the loader
        // then fails to find a library that is present.
        assert_eq!(soname(&path).as_deref(), Some("libnvidia-nvvm70.so.4"));
    }

    #[test]
    fn a_file_with_no_soname_is_not_an_error() {
        let dir = TempTree::new(&["notelf"]);
        assert_eq!(soname(&dir.path().join("notelf")), None);
        assert_eq!(soname(Path::new("/nonexistent/at/all")), None);
    }

    #[test]
    fn a_manifest_that_is_not_an_array_is_rejected() {
        assert!(parse_manifest("{}").is_err());
        assert!(parse_manifest("[{\"type\": \"LIB\"}]").is_err(), "an entry with no name");
    }

    /// A directory of empty files, removed on drop.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(files: &[&str]) -> Self {
            let base = std::env::temp_dir().join(format!(
                "nvgpu-userspace-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("mkdir base");
            for f in files {
                let p = base.join(f);
                std::fs::create_dir_all(p.parent().expect("a parent")).expect("mkdir");
                std::fs::write(&p, b"").expect("write");
            }
            Self(base)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
