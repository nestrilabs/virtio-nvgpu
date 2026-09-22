//! Stage the host's NVIDIA user-mode driver for a guest to mount.
//!
//! A guest image must not carry its own copy of the driver userspace: the
//! ioctls this project forwards are a private contract between one build of
//! `libcuda` and one build of the host kernel module, so the only way for the
//! two to be in step is for them to be the same files. This walks the driver's
//! own manifest, finds what a guest needs on this host, and builds a directory
//! to export read-only over a filesystem share.
//!
//!     nvgpu-userspace                      # what this host would export
//!     nvgpu-userspace --stage /run/nvgpu   # build the share
//!
//! The staged tree is hard links where the filesystem allows it, so exporting
//! ~1 GiB of driver costs no extra space and no copy. It falls back to symlinks
//! across filesystems, which a share can still follow because the target is
//! resolved on the host side.

use anyhow::{Context, Result};
use clap::Parser;
use device::userspace::{
    load_manifest, loaded_driver_version, resolve, staged_driver_version, Capability,
    DEFAULT_MANIFEST, DEFAULT_SEARCH_PATHS, LOADED_VERSION_PATH,
};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(about = "Stage the host NVIDIA user-mode driver for a guest")]
struct Args {
    /// The driver's file manifest.
    #[arg(long, default_value = DEFAULT_MANIFEST)]
    manifest: PathBuf,

    /// Build the share here. Without this, nothing is written.
    #[arg(long)]
    stage: Option<PathBuf>,

    /// Which capabilities to carry: utility, compute, graphics, video.
    #[arg(long, value_delimiter = ',', default_value = "utility,compute,graphics,video")]
    caps: Vec<String>,

    /// List every file, not just the totals.
    #[arg(long)]
    verbose: bool,

    /// Stage even when the files found are from a different driver than the
    /// kernel module that is loaded. Almost always the wrong thing: see the
    /// refusal this suppresses.
    #[arg(long)]
    allow_version_mismatch: bool,
}

fn capability(name: &str) -> Result<Capability> {
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "utility" => Capability::Utility,
        "compute" => Capability::Compute,
        "graphics" => Capability::Graphics,
        "video" => Capability::Video,
        other => anyhow::bail!("unknown capability {other:?} (utility, compute, graphics, video)"),
    })
}

/// Place `src` into the share at `dst`, preferring a hard link and copying
/// when the two are on different filesystems.
///
/// Deliberately never a symlink. A symlink here would point at an absolute
/// host path, and the guest resolves the share's symlinks in its *own*
/// namespace, where `/usr/lib/libcuda.so...` does not exist. The share would
/// then look complete on the host and be empty from inside the VM -- which is
/// exactly what happened when this staged into /run, a tmpfs, and every link
/// fell back to a symlink.
///
/// So EXDEV costs a real copy of ~800 MiB. Staging onto the same filesystem as
/// the driver keeps it free, and [`main`] says so when it is not.
fn place(src: &Path, dst: &Path) -> Result<&'static str> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok("hardlink"),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            // Resolve first: a manifest entry may itself be a symlink on the
            // host, and copying the link rather than its target reintroduces
            // the dangling-path problem this function exists to avoid.
            let target = std::fs::canonicalize(src)
                .with_context(|| format!("resolving {}", src.display()))?;
            std::fs::copy(&target, dst)
                .with_context(|| format!("copying {} to {}", target.display(), dst.display()))?;
            Ok("copy")
        }
        Err(e) => Err(e).with_context(|| format!("linking {}", dst.display())),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let caps = args
        .caps
        .iter()
        .map(|c| capability(c))
        .collect::<Result<Vec<_>>>()?;

    let entries = load_manifest(&args.manifest)?;
    let search: Vec<PathBuf> = DEFAULT_SEARCH_PATHS.iter().map(PathBuf::from).collect();
    let (found, missing) = resolve(&entries, &caps, &search);

    let bytes: u64 = found
        .iter()
        .filter_map(|r| std::fs::metadata(&r.host_path).ok())
        .map(|m| m.len())
        .sum();

    println!(
        "{} entries in the manifest, {} wanted here, {:.1} MiB",
        entries.len(),
        found.len(),
        bytes as f64 / (1024.0 * 1024.0)
    );

    if args.verbose {
        for r in &found {
            println!("  {:<10} {}", r.entry.kind.to_string(), r.guest_path.display());
        }
    }

    // Absent files are normal -- a manifest covers optional packages too -- so
    // they are named rather than treated as a failure. Naming them is what
    // turns "Vulkan does not work in the guest" into "the ICD was never there".
    if !missing.is_empty() {
        println!("\n{} listed but not installed on this host:", missing.len());
        for e in &missing {
            println!("  {} ({})", e.name, e.kind);
        }
    }

    // What the manifest led us to, and what is actually loaded. These agree on
    // a host with one driver install and can differ on a host with two -- and
    // only the loaded module's own userspace can speak to it.
    let staged_version = staged_driver_version(&found);
    let loaded_version = loaded_driver_version(Path::new(LOADED_VERSION_PATH));
    match (&staged_version, &loaded_version) {
        (Some(s), Some(l)) if s == l => println!("driver {s}, matching the loaded module"),
        (Some(s), Some(l)) => println!("manifest describes driver {s}; the loaded module is {l}"),
        (Some(s), None) => println!("driver {s}; no module loaded to check it against"),
        _ => {}
    }

    let Some(root) = args.stage else {
        println!("\nNothing written. Pass --stage <dir> to build the share.");
        return Ok(());
    };

    // Refuse before writing, not after. A share built from the wrong driver is
    // the one failure here that does not look like one: it mounts, every
    // forwarded ioctl returns 0, and the caller gives up deep inside a library
    // that is a different build from the kernel module it is talking to.
    if let (Some(staged), Some(loaded)) = (&staged_version, &loaded_version) {
        if staged != loaded && !args.allow_version_mismatch {
            anyhow::bail!(
                "refusing to stage: the manifest at {} describes driver {staged}, but the \n\
                 loaded kernel module is {loaded}. The guest runs these libraries against \n\
                 that module, and the forwarded ioctls are a private contract between one \n\
                 build of each -- so this share would fail deep inside the guest instead of \n\
                 here.\n\n\
                 This host has two driver userspaces installed. Point --manifest at the one \n\
                 describing {loaded}, or remove the {staged} install. --allow-version-mismatch \n\
                 overrides this if you are deliberately testing the mismatch.",
                args.manifest.display()
            );
        }
    }

    // A stale share is worse than no share: it would hold libraries from a
    // driver that is no longer loaded, which fails deep inside the guest rather
    // than here.
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .with_context(|| format!("clearing the previous share at {}", root.display()))?;
    }

    // Which file names the share will actually contain. A symlink entry is
    // only worth staging if whatever it points at is one of them.
    let staged_names: std::collections::BTreeSet<_> = found
        .iter()
        .filter_map(|r| r.guest_path.file_name().map(|n| n.to_owned()))
        .collect();

    let mut hard = 0usize;
    let mut copied = 0usize;
    let mut dangling = Vec::new();

    for r in &found {
        // A manifest symlink can point outside the driver. On this host
        // libGLX_indirect.so.0 points at Mesa's GLX, which no NVIDIA manifest
        // lists, so staging it yields a broken link in the share -- something
        // that looks present, resolves to nothing, and is found much later.
        if let Ok(target) = std::fs::read_link(&r.host_path) {
            let wanted = target.file_name().map(|n| n.to_owned());
            if !wanted.is_some_and(|n| staged_names.contains(&n)) {
                dangling.push((r.guest_path.clone(), target));
                continue;
            }
        }
        match place(&r.host_path, &root.join(&r.guest_path))? {
            "copy" => copied += 1,
            _ => hard += 1,
        }
    }

    // The manifest names real files; the soname links a caller dlopens are made
    // by packaging and are not in it. Without them the share holds every
    // library and resolves none, which a guest reports as the library being
    // absent while it is sitting in the mount.
    //
    // These are relative links on purpose: an absolute one would point at a
    // host path the guest cannot follow.
    let mut linked = 0usize;
    for r in &found {
        let staged = root.join(&r.guest_path);
        let Some(soname) = device::userspace::soname(&staged) else {
            continue;
        };
        let Some(real) = r.guest_path.file_name() else {
            continue;
        };
        if soname.as_str() == real {
            continue;
        }
        let link = staged.with_file_name(&soname);
        if link.exists() {
            continue;
        }
        std::os::unix::fs::symlink(real, &link)
            .with_context(|| format!("linking {} -> {:?}", link.display(), real))?;
        linked += 1;
    }

    println!(
        "\nStaged {} files at {} ({hard} hard linked, {copied} copied, {linked} soname links).",
        found.len(),
        root.display()
    );
    if copied > 0 {
        println!(
            "{} files were on another filesystem and cost a real copy. Staging under \
             the same filesystem as the driver makes the share free.",
            copied
        );
    }
    for (path, target) in &dangling {
        println!(
            "  skipped {}: points at {}, which is not part of this driver",
            path.display(),
            target.display()
        );
    }
    println!("Export it read-only and, in the guest, add its lib/ to the loader path.");
    Ok(())
}
