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

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

/// The driver's own manifest, on a host where one is installed.
pub const DEFAULT_MANIFEST: &str = "/usr/share/nvidia/files.d/sandboxutils-filelist.json";

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

#[cfg(test)]
mod tests {
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
