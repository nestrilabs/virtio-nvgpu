//! The hand-written half of the proxy's knowledge: which globals a guest may
//! see, and what every descriptor they can carry is.
//!
//! This file is plain data, and it is compiled twice: once into the library,
//! and once into `build.rs`, which checks it against the vendored protocol XML
//! before a single line of the codec is generated. That check is the point of
//! keeping the table here rather than in code: every global below is walked
//! for everything it can create (typed `new_id`s in requests and events, to a
//! fixed point), and the build fails if anything reachable carries a
//! descriptor that has no entry in `FD_POLICY`. Nothing is forwarded on trust:
//! libwayland hands descriptors out of one FIFO per connection, in the order
//! the messages that consume them are demarshalled, so a single message whose
//! descriptors the proxy did not count desynchronises every later one.
//!
//! Adding a protocol is: vendor its XML under `protocols/`, add its global to
//! `GLOBALS`, and -- only if the build says so -- one line to `FD_POLICY`.

/// What a descriptor argument is, and so how it crosses the VM boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum FdClass {
    /// `wl_shm.create_pool`: the far side owns a memfd of the pool's size,
    /// kept filled from the near side's pool at every commit that shows it.
    ShmPool,
    /// A dma-buf plane. Guest to host it is the host GEM object behind the
    /// guest proxy, PRIME-exported on the host; host to guest (export mode) it
    /// is imported into the channel's render file.
    Dmabuf,
    /// A small read-only file, copied by value into a sealed memfd on the far
    /// side. `size` names the argument that says how many bytes to take;
    /// `offset`, if any, where to start (it is rewritten to 0 on the way out).
    Blob {
        size: &'static str,
        offset: Option<&'static str>,
    },
    /// The write end of a pipe the peer will write into: a credit-controlled
    /// byte stream with an explicit end.
    Stream,
    /// A DRM file of the host GPU (a lease, or the lease device's query fd):
    /// adopted into a guest DRM file by the guest kernel.
    DrmFile,
    /// A DRM syncobj. Refused until fences are bridged.
    Syncobj,
}

/// What a global needs before it may be offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Requires {
    /// Nothing: always offered when the host compositor has it.
    Nothing,
    /// DRM files can be adopted on the guest side, and the host's lease device
    /// hands out files of *our* GPU (checked per global by the host).
    DrmFile,
    /// Fence bridging (syncobj proxies). Not yet available.
    Fences,
}

/// One global a guest may bind.
#[derive(Clone, Copy, Debug)]
pub struct GlobalSpec {
    pub interface: &'static str,
    /// Highest version the proxy has been reviewed for; `0` means "whatever
    /// the vendored XML says". The version offered is min(host, XML, this).
    pub max_version: u32,
    pub requires: Requires,
}

const fn g(interface: &'static str) -> GlobalSpec {
    GlobalSpec {
        interface,
        max_version: 0,
        requires: Requires::Nothing,
    }
}
const fn gv(interface: &'static str, max_version: u32) -> GlobalSpec {
    GlobalSpec {
        interface,
        max_version,
        requires: Requires::Nothing,
    }
}
const fn gr(interface: &'static str, requires: Requires) -> GlobalSpec {
    GlobalSpec {
        interface,
        max_version: 0,
        requires,
    }
}

/// The exhaustive allowlist. Anything not here is dropped from the registry
/// and refused at bind.
///
/// The base is Hyprland's own sandbox whitelist -- the set it gives a client
/// it does not trust (`src/managers/ProtocolManager.cpp:348-403` in 0.56) --
/// because that is exactly what a guest application is. On top of it:
/// `wl_output` and `zxdg_output_manager_v1` (dynamic in Hyprland, so absent
/// from its list), content-type (so a guest game can ask for direct scanout),
/// and the explicit opt-ins with a requirement. Deliberately absent: security
/// context (a listening socket), input capture (an EIS socket), gamma, the
/// data-control managers (a guest clipboard manager would see the host
/// clipboard), virtual keyboard and pointer, input method, every capture
/// protocol (the compositor writes into client memory there), layer shell,
/// session lock, output management, and `wl_drm` (a host device path, and
/// Hyprland picks the wrong modifier for its buffers).
pub const GLOBALS: &[GlobalSpec] = &[
    // core
    g("wl_compositor"),
    g("wl_subcompositor"),
    g("wl_shm"),
    g("wl_seat"),
    g("wl_output"),
    g("wl_data_device_manager"),
    // Hyprland's sandbox whitelist
    g("wp_viewporter"),
    g("wp_tearing_control_manager_v1"),
    g("wp_fractional_scale_manager_v1"),
    g("wp_cursor_shape_manager_v1"),
    g("zwp_idle_inhibit_manager_v1"),
    g("zwp_relative_pointer_manager_v1"),
    g("zxdg_decoration_manager_v1"),
    g("wp_alpha_modifier_v1"),
    g("zwp_pointer_gestures_v1"),
    g("zwp_keyboard_shortcuts_inhibit_manager_v1"),
    g("zwp_text_input_manager_v1"),
    g("zwp_text_input_manager_v3"),
    g("zwp_pointer_constraints_v1"),
    g("xdg_activation_v1"),
    g("ext_idle_notifier_v1"),
    g("org_kde_kwin_server_decoration_manager"),
    g("zwp_tablet_manager_v2"),
    g("wp_presentation"),
    g("xdg_wm_base"),
    g("xdg_wm_dialog_v1"),
    g("wp_single_pixel_buffer_manager_v1"),
    g("zwp_primary_selection_device_manager_v1"),
    g("hyprland_surface_manager_v1"),
    g("xdg_toplevel_tag_manager_v1"),
    g("xdg_system_bell_v1"),
    g("wp_fifo_manager_v1"),
    g("wp_commit_timing_manager_v1"),
    g("zxdg_exporter_v2"),
    g("zxdg_importer_v2"),
    g("ext_background_effect_manager_v1"),
    // v6 adds set_sampling_device, a dev_t the guest would send us; not
    // reviewed, and Hyprland offers 5.
    gv("zwp_linux_dmabuf_v1", 5),
    g("wp_color_manager_v1"),
    // added to the sandbox set
    g("zxdg_output_manager_v1"),
    g("wp_content_type_manager_v1"),
    // opt-ins with a requirement
    gr("wp_drm_lease_device_v1", Requires::DrmFile),
    gr("wp_linux_drm_syncobj_manager_v1", Requires::Fences),
];

/// Every descriptor-carrying message any allowed global can reach, by
/// (interface, message). Direction follows from whether the XML calls it a
/// request or an event.
pub const FD_POLICY: &[(&str, &str, FdClass)] = &[
    ("wl_shm", "create_pool", FdClass::ShmPool),
    ("wl_data_offer", "receive", FdClass::Stream),
    ("wl_data_source", "send", FdClass::Stream),
    ("zwp_primary_selection_offer_v1", "receive", FdClass::Stream),
    ("zwp_primary_selection_source_v1", "send", FdClass::Stream),
    (
        "wl_keyboard",
        "keymap",
        FdClass::Blob {
            size: "size",
            offset: None,
        },
    ),
    ("zwp_linux_buffer_params_v1", "add", FdClass::Dmabuf),
    (
        "zwp_linux_dmabuf_feedback_v1",
        "format_table",
        FdClass::Blob {
            size: "size",
            offset: None,
        },
    ),
    (
        "wp_image_description_creator_icc_v1",
        "set_icc_file",
        FdClass::Blob {
            size: "length",
            offset: Some("offset"),
        },
    ),
    (
        "wp_image_description_info_v1",
        "icc_file",
        FdClass::Blob {
            size: "icc_size",
            offset: None,
        },
    ),
    ("wp_drm_lease_device_v1", "drm_fd", FdClass::DrmFile),
    ("wp_drm_lease_v1", "lease_fd", FdClass::DrmFile),
    (
        "wp_linux_drm_syncobj_manager_v1",
        "import_timeline",
        FdClass::Syncobj,
    ),
];

/// Non-descriptor arguments that mean something different on each side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Rewrite {
    /// An array holding one `dev_t` of a DRM node: host numbers are mapped to
    /// the guest's node for the same GPU (the guest kernel supplies the map).
    DevT { arg: &'static str },
    /// A CLOCK_MONOTONIC timestamp split as (tv_sec_hi, tv_sec_lo, tv_nsec):
    /// moved by the guest-host clock offset.
    Timestamp {
        sec_hi: &'static str,
        sec_lo: &'static str,
        nsec: &'static str,
    },
    /// The presentation clock in use; timestamps are only moved when it is one
    /// both clocks share an offset for.
    ClockId { arg: &'static str },
}

pub const REWRITES: &[(&str, &str, Rewrite)] = &[
    (
        "zwp_linux_dmabuf_feedback_v1",
        "main_device",
        Rewrite::DevT { arg: "device" },
    ),
    (
        "zwp_linux_dmabuf_feedback_v1",
        "tranche_target_device",
        Rewrite::DevT { arg: "device" },
    ),
    (
        "ext_image_copy_capture_session_v1",
        "dmabuf_device",
        Rewrite::DevT { arg: "device" },
    ),
    (
        "wp_presentation_feedback",
        "presented",
        Rewrite::Timestamp {
            sec_hi: "tv_sec_hi",
            sec_lo: "tv_sec_lo",
            nsec: "tv_nsec",
        },
    ),
    (
        "wp_commit_timer_v1",
        "set_timestamp",
        Rewrite::Timestamp {
            sec_hi: "tv_sec_hi",
            sec_lo: "tv_sec_lo",
            nsec: "tv_nsec",
        },
    ),
    (
        "wp_presentation",
        "clock_id",
        Rewrite::ClockId { arg: "clk_id" },
    ),
];
