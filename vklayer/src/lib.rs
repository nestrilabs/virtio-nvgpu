//! A guest Vulkan layer that stops offering what the guest cannot create.
//!
//! NVIDIA's Vulkan driver in a virtio-nvgpu guest lists acceleration
//! structures and the ray tracing built on them, binary import, optical flow
//! and CUDA kernel launch, then fails `vkCreateDevice` with
//! `VK_ERROR_INITIALIZATION_FAILED` when any of them is asked for. An
//! application that enables what it is offered -- most engines with a ray
//! tracing path -- then does not start. `scripts/rig/guest/vkrt.dyn.c`
//! measured it on box 2 (RTX 3060, 595.104.02): all seven fail in the guest
//! with compute off and with compute on, and all seven succeed on the host.
//!
//! The driver needs UVM for them. Without compute there is no
//! `/dev/nvidia-uvm`; with it, the UVM path is M5's and not yet enough.
//! So the layer hides them, and their feature bits, and refuses a device
//! that asks for them anyway with `VK_ERROR_EXTENSION_NOT_PRESENT`, which an
//! application handles, instead of the driver's failure, which it does not.
//!
//! Nothing here is a security boundary: the backend's checks are. This only
//! makes the guest's Vulkan say what it can do.
//!
//! No Vulkan crate: the few types the loader interface needs are below,
//! from `vulkan_core.h` and `vk_layer.h`.

#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;

type VkResult = i32;
const VK_SUCCESS: VkResult = 0;
const VK_INCOMPLETE: VkResult = 5;
const VK_ERROR_INITIALIZATION_FAILED: VkResult = -3;
const VK_ERROR_LAYER_NOT_PRESENT: VkResult = -6;
const VK_ERROR_EXTENSION_NOT_PRESENT: VkResult = -7;

type Handle = *mut c_void;
type PFN_vkVoidFunction = Option<unsafe extern "system" fn()>;
type PFN_vkGetInstanceProcAddr =
    unsafe extern "system" fn(Handle, *const c_char) -> PFN_vkVoidFunction;
type PFN_vkGetDeviceProcAddr = PFN_vkGetInstanceProcAddr;
type PFN_vkCreateInstance =
    unsafe extern "system" fn(*const c_void, *const c_void, *mut Handle) -> VkResult;
type PFN_vkDestroyInstance = unsafe extern "system" fn(Handle, *const c_void);
type PFN_vkCreateDevice = unsafe extern "system" fn(
    Handle,
    *const DeviceCreateInfo,
    *const c_void,
    *mut Handle,
) -> VkResult;
type PFN_vkEnumerateDeviceExtensionProperties = unsafe extern "system" fn(
    Handle,
    *const c_char,
    *mut u32,
    *mut ExtensionProperties,
) -> VkResult;
type PFN_vkGetPhysicalDeviceFeatures2 = unsafe extern "system" fn(Handle, *mut BaseOut);

const LAYER_NAME: &CStr = c"VK_LAYER_NVGPU_no_uvm";

/// Extensions the guest is offered and cannot create, and the ones that
/// require them. An extension whose requirement is hidden has to go too, or
/// an application enables it alone and gets the driver's failure.
const HIDDEN: &[&str] = &[
    // Measured failing by vkrt.
    "VK_KHR_acceleration_structure",
    "VK_KHR_ray_query",
    "VK_KHR_ray_tracing_pipeline",
    "VK_NV_ray_tracing",
    "VK_NVX_binary_import",
    "VK_NV_optical_flow",
    "VK_NV_cuda_kernel_launch",
    // Require one of the above.
    "VK_KHR_ray_tracing_maintenance1",
    "VK_KHR_ray_tracing_position_fetch",
    "VK_NV_ray_tracing_motion_blur",
    "VK_NV_ray_tracing_invocation_reorder",
    "VK_EXT_ray_tracing_invocation_reorder",
    "VK_NV_ray_tracing_validation",
    "VK_NV_ray_tracing_linear_swept_spheres",
    "VK_NV_cluster_acceleration_structure",
    "VK_NV_partitioned_acceleration_structure",
    "VK_EXT_opacity_micromap",
    "VK_NV_displacement_micromap",
];

/// Feature structures of hidden extensions, and how many `VkBool32`s follow
/// their header. Zeroed, so an application that checks a feature rather
/// than an extension also sees it absent.
const FEATURES: &[(i32, usize)] = &[
    (1000150013, 5), // AccelerationStructureFeaturesKHR
    (1000347000, 5), // RayTracingPipelineFeaturesKHR
    (1000348013, 1), // RayQueryFeaturesKHR
    (1000464000, 1), // OpticalFlowFeaturesNV
    (1000307000, 1), // CudaKernelLaunchFeaturesNV
];

/// Whether a guest's UVM serves what these extensions need. Not yet: vkrt
/// fails them with compute on too. M5 changes this once vkrt passes with
/// the layer disabled.
const UVM_SERVES_VULKAN: bool = false;

fn hiding() -> bool {
    !UVM_SERVES_VULKAN || !std::path::Path::new("/dev/nvidia-uvm").exists()
}

pub fn is_hidden(name: &[u8]) -> bool {
    HIDDEN.iter().any(|h| h.as_bytes() == name)
}

// ── loader interface (vk_layer.h) ──

const LOADER_INSTANCE_CREATE_INFO: i32 = 47;
const LOADER_DEVICE_CREATE_INFO: i32 = 48;
const VK_LAYER_LINK_INFO: i32 = 0;

#[repr(C)]
struct BaseOut {
    s_type: i32,
    p_next: *mut BaseOut,
}

#[repr(C)]
struct LayerInstanceLink {
    p_next: *mut LayerInstanceLink,
    next_gipa: PFN_vkGetInstanceProcAddr,
    next_gpdpa: PFN_vkVoidFunction,
}

#[repr(C)]
struct LayerDeviceLink {
    p_next: *mut LayerDeviceLink,
    next_gipa: PFN_vkGetInstanceProcAddr,
    next_gdpa: PFN_vkGetDeviceProcAddr,
}

/// VkLayerInstanceCreateInfo and VkLayerDeviceCreateInfo share this shape
/// for VK_LAYER_LINK_INFO: the union's first member is the link pointer.
#[repr(C)]
struct LayerCreateInfo<L> {
    s_type: i32,
    p_next: *mut c_void,
    function: i32,
    link: *mut L,
}

#[repr(C)]
pub struct DeviceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_create_info_count: u32,
    p_queue_create_infos: *const c_void,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
    p_enabled_features: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExtensionProperties {
    pub name: [c_char; 256],
    pub spec_version: u32,
}

#[repr(C)]
pub struct NegotiateLayerInterface {
    s_type: i32,
    p_next: *mut c_void,
    version: u32,
    gipa: Option<PFN_vkGetInstanceProcAddr>,
    gdpa: Option<PFN_vkGetDeviceProcAddr>,
    gpdpa: PFN_vkVoidFunction,
}

// ── per-instance and per-device state, by dispatch key ──

/// A dispatchable handle's first word is the loader's dispatch table; an
/// instance's physical devices share their instance's.
unsafe fn key(h: Handle) -> usize {
    unsafe { *(h as *const usize) }
}

#[derive(Clone, Copy)]
struct Instance {
    key: usize,
    /// The instance's handle as the next layer knows it.
    handle: usize,
    gipa: PFN_vkGetInstanceProcAddr,
    destroy: PFN_vkDestroyInstance,
    enum_ext: PFN_vkEnumerateDeviceExtensionProperties,
    features2: Option<PFN_vkGetPhysicalDeviceFeatures2>,
}

static INSTANCES: Mutex<Vec<Instance>> = Mutex::new(Vec::new());
static DEVICES: Mutex<Vec<(usize, PFN_vkGetDeviceProcAddr)>> = Mutex::new(Vec::new());

fn instance(k: usize) -> Option<Instance> {
    INSTANCES
        .lock()
        .unwrap()
        .iter()
        .find(|i| i.key == k)
        .copied()
}

unsafe fn find_link<L>(mut p: *const c_void, s_type: i32) -> *mut LayerCreateInfo<L> {
    while !p.is_null() {
        let c = p as *mut LayerCreateInfo<L>;
        unsafe {
            if (*c).s_type == s_type && (*c).function == VK_LAYER_LINK_INFO {
                return c;
            }
            p = (*c).p_next;
        }
    }
    std::ptr::null_mut()
}

unsafe fn load<F>(gipa: PFN_vkGetInstanceProcAddr, h: Handle, name: &CStr) -> Option<F> {
    unsafe { gipa(h, name.as_ptr()).map(|f| std::mem::transmute_copy(&f)) }
}

// ── entry points ──

/// # Safety
/// Called by the Vulkan loader with a valid negotiation structure.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    n: *mut NegotiateLayerInterface,
) -> VkResult {
    let n = unsafe { &mut *n };
    if n.version < 2 {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    n.version = 2;
    n.gipa = Some(get_instance_proc_addr);
    n.gdpa = Some(get_device_proc_addr);
    n.gpdpa = None;
    VK_SUCCESS
}

macro_rules! fp {
    ($f:expr, $t:ty) => {
        Some(unsafe { std::mem::transmute::<$t, unsafe extern "system" fn()>($f) })
    };
}

unsafe extern "system" fn get_instance_proc_addr(
    inst: Handle,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    let n = unsafe { CStr::from_ptr(name) }.to_bytes();
    match n {
        b"vkGetInstanceProcAddr" => {
            return fp!(
                get_instance_proc_addr as PFN_vkGetInstanceProcAddr,
                PFN_vkGetInstanceProcAddr
            );
        }
        b"vkCreateInstance" => {
            return fp!(
                create_instance as PFN_vkCreateInstance,
                PFN_vkCreateInstance
            );
        }
        b"vkDestroyInstance" => {
            return fp!(
                destroy_instance as PFN_vkDestroyInstance,
                PFN_vkDestroyInstance
            );
        }
        b"vkCreateDevice" => return fp!(create_device as PFN_vkCreateDevice, PFN_vkCreateDevice),
        b"vkEnumerateDeviceExtensionProperties" => {
            return fp!(
                enumerate_device_extensions as PFN_vkEnumerateDeviceExtensionProperties,
                PFN_vkEnumerateDeviceExtensionProperties
            );
        }
        b"vkGetPhysicalDeviceFeatures2" | b"vkGetPhysicalDeviceFeatures2KHR" => {
            return fp!(
                features2 as PFN_vkGetPhysicalDeviceFeatures2,
                PFN_vkGetPhysicalDeviceFeatures2
            );
        }
        b"vkGetDeviceProcAddr" => {
            return fp!(
                get_device_proc_addr as PFN_vkGetDeviceProcAddr,
                PFN_vkGetDeviceProcAddr
            );
        }
        _ => {}
    }
    if inst.is_null() {
        return None;
    }
    let i = instance(unsafe { key(inst) })?;
    unsafe { (i.gipa)(inst, name) }
}

unsafe extern "system" fn get_device_proc_addr(
    dev: Handle,
    name: *const c_char,
) -> PFN_vkVoidFunction {
    if unsafe { CStr::from_ptr(name) }.to_bytes() == b"vkGetDeviceProcAddr" {
        return fp!(
            get_device_proc_addr as PFN_vkGetDeviceProcAddr,
            PFN_vkGetDeviceProcAddr
        );
    }
    let k = unsafe { key(dev) };
    let next = DEVICES
        .lock()
        .unwrap()
        .iter()
        .find(|d| d.0 == k)
        .map(|d| d.1)?;
    unsafe { next(dev, name) }
}

unsafe extern "system" fn create_instance(
    ci: *const c_void,
    alloc: *const c_void,
    out: *mut Handle,
) -> VkResult {
    unsafe {
        let chain = find_link::<LayerInstanceLink>(
            *(ci as *const *const c_void).add(1),
            LOADER_INSTANCE_CREATE_INFO,
        );
        if chain.is_null() || (*chain).link.is_null() {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let gipa = (*(*chain).link).next_gipa;
        (*chain).link = (*(*chain).link).p_next;
        let Some(create) =
            load::<PFN_vkCreateInstance>(gipa, std::ptr::null_mut(), c"vkCreateInstance")
        else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let r = create(ci, alloc, out);
        if r != VK_SUCCESS {
            return r;
        }
        let inst = *out;
        let (Some(destroy), Some(enum_ext)) = (
            load(gipa, inst, c"vkDestroyInstance"),
            load(gipa, inst, c"vkEnumerateDeviceExtensionProperties"),
        ) else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let features2 = load(gipa, inst, c"vkGetPhysicalDeviceFeatures2")
            .or_else(|| load(gipa, inst, c"vkGetPhysicalDeviceFeatures2KHR"));
        INSTANCES.lock().unwrap().push(Instance {
            key: key(inst),
            handle: inst as usize,
            gipa,
            destroy,
            enum_ext,
            features2,
        });
        r
    }
}

unsafe extern "system" fn destroy_instance(inst: Handle, alloc: *const c_void) {
    if inst.is_null() {
        return;
    }
    let k = unsafe { key(inst) };
    let i = {
        let mut all = INSTANCES.lock().unwrap();
        let Some(pos) = all.iter().position(|i| i.key == k) else {
            return;
        };
        all.remove(pos)
    };
    unsafe { (i.destroy)(inst, alloc) }
}

unsafe extern "system" fn enumerate_device_extensions(
    pd: Handle,
    layer: *const c_char,
    count: *mut u32,
    props: *mut ExtensionProperties,
) -> VkResult {
    unsafe {
        if !layer.is_null() {
            if CStr::from_ptr(layer) == LAYER_NAME {
                *count = 0;
                return VK_SUCCESS;
            }
            let Some(i) = instance(key(pd)) else {
                return VK_ERROR_LAYER_NOT_PRESENT;
            };
            return (i.enum_ext)(pd, layer, count, props);
        }
        let Some(i) = instance(key(pd)) else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        if !hiding() {
            return (i.enum_ext)(pd, layer, count, props);
        }
        let mut n = 0u32;
        let r = (i.enum_ext)(pd, layer, &mut n, std::ptr::null_mut());
        if r != VK_SUCCESS {
            return r;
        }
        let mut all = vec![
            ExtensionProperties {
                name: [0; 256],
                spec_version: 0
            };
            n as usize
        ];
        let r = (i.enum_ext)(pd, layer, &mut n, all.as_mut_ptr());
        if r < 0 {
            return r;
        }
        all.truncate(n as usize);
        let kept = filter(all);
        copy_out(&kept, count, props)
    }
}

pub fn filter(all: Vec<ExtensionProperties>) -> Vec<ExtensionProperties> {
    all.into_iter()
        .filter(|e| {
            let name = unsafe { CStr::from_ptr(e.name.as_ptr()) };
            !is_hidden(name.to_bytes())
        })
        .collect()
}

/// The two-call idiom: a null `props` asks for the count; otherwise write at
/// most `*count` and say VK_INCOMPLETE if that was not all.
///
/// # Safety
/// `count` must be valid, and `props`, when not null, hold `*count` entries.
pub unsafe fn copy_out(
    kept: &[ExtensionProperties],
    count: *mut u32,
    props: *mut ExtensionProperties,
) -> VkResult {
    unsafe {
        if props.is_null() {
            *count = kept.len() as u32;
            return VK_SUCCESS;
        }
        let n = (*count as usize).min(kept.len());
        std::ptr::copy_nonoverlapping(kept.as_ptr(), props, n);
        *count = n as u32;
        if n < kept.len() {
            VK_INCOMPLETE
        } else {
            VK_SUCCESS
        }
    }
}

unsafe extern "system" fn features2(pd: Handle, out: *mut BaseOut) {
    unsafe {
        let Some(f) = instance(key(pd)).and_then(|i| i.features2) else {
            return;
        };
        f(pd, out);
        if hiding() {
            clear_features(out as *mut c_void);
        }
    }
}

/// Zero the feature bits of hidden extensions in an output chain.
///
/// # Safety
/// `p` must be a valid Vulkan output structure chain.
pub unsafe fn clear_features(mut p: *mut c_void) {
    while !p.is_null() {
        let b = p as *mut BaseOut;
        unsafe {
            if let Some(&(_, n)) = FEATURES.iter().find(|f| f.0 == (*b).s_type) {
                let bools = (b as *mut u8).add(std::mem::size_of::<BaseOut>()) as *mut u32;
                for i in 0..n {
                    *bools.add(i) = 0;
                }
            }
            p = (*b).p_next as *mut c_void;
        }
    }
}

unsafe extern "system" fn create_device(
    pd: Handle,
    ci: *const DeviceCreateInfo,
    alloc: *const c_void,
    out: *mut Handle,
) -> VkResult {
    unsafe {
        if hiding() && asks_hidden(&*ci) {
            return VK_ERROR_EXTENSION_NOT_PRESENT;
        }
        let chain = find_link::<LayerDeviceLink>((*ci).p_next, LOADER_DEVICE_CREATE_INFO);
        if chain.is_null() || (*chain).link.is_null() {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        let link = &*(*chain).link;
        let (gipa, gdpa) = (link.next_gipa, link.next_gdpa);
        (*chain).link = link.p_next;
        let Some(inst) = instance(key(pd)) else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let Some(create) =
            load::<PFN_vkCreateDevice>(gipa, inst.handle as Handle, c"vkCreateDevice")
        else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let r = create(pd, ci, alloc, out);
        if r == VK_SUCCESS {
            DEVICES.lock().unwrap().push((key(*out), gdpa));
        }
        r
    }
}

/// # Safety
/// `ci`'s extension list must be valid C strings, as Vulkan requires.
pub unsafe fn asks_hidden(ci: &DeviceCreateInfo) -> bool {
    (0..ci.enabled_extension_count as usize).any(|i| {
        let name = unsafe { CStr::from_ptr(*ci.pp_enabled_extension_names.add(i)) };
        is_hidden(name.to_bytes())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(name: &str) -> ExtensionProperties {
        let mut e = ExtensionProperties {
            name: [0; 256],
            spec_version: 1,
        };
        for (d, s) in e.name.iter_mut().zip(name.bytes()) {
            *d = s as c_char;
        }
        e
    }

    fn names(v: &[ExtensionProperties]) -> Vec<String> {
        v.iter()
            .map(|e| {
                unsafe { CStr::from_ptr(e.name.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn every_extension_vkrt_saw_fail_is_hidden() {
        for e in [
            "VK_KHR_acceleration_structure",
            "VK_KHR_ray_query",
            "VK_KHR_ray_tracing_pipeline",
            "VK_NV_ray_tracing",
            "VK_NVX_binary_import",
            "VK_NV_optical_flow",
            "VK_NV_cuda_kernel_launch",
        ] {
            assert!(is_hidden(e.as_bytes()), "{e}");
        }
    }

    /// A prefix of a hidden name is a different extension.
    #[test]
    fn names_match_whole() {
        assert!(!is_hidden(b"VK_KHR_ray_query_x"));
        assert!(!is_hidden(b"VK_KHR_ray"));
        assert!(!is_hidden(b"VK_KHR_deferred_host_operations"));
    }

    #[test]
    fn the_rest_pass_in_order() {
        let all = vec![
            ext("VK_KHR_swapchain"),
            ext("VK_KHR_ray_query"),
            ext("VK_EXT_mesh_shader"),
        ];
        assert_eq!(
            names(&filter(all)),
            ["VK_KHR_swapchain", "VK_EXT_mesh_shader"]
        );
    }

    #[test]
    fn a_short_buffer_is_incomplete() {
        let kept = vec![ext("a"), ext("b"), ext("c")];
        let mut n = 0;
        assert_eq!(
            unsafe { copy_out(&kept, &mut n, std::ptr::null_mut()) },
            VK_SUCCESS
        );
        assert_eq!(n, 3);
        let mut buf = [ext(""); 2];
        n = 2;
        assert_eq!(
            unsafe { copy_out(&kept, &mut n, buf.as_mut_ptr()) },
            VK_INCOMPLETE
        );
        assert_eq!(
            (n, names(&buf)),
            (2, vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn hidden_feature_bits_are_cleared_and_others_kept() {
        #[repr(C)]
        struct As {
            base: BaseOut,
            bools: [u32; 5],
        }
        #[repr(C)]
        struct Other {
            base: BaseOut,
            bools: [u32; 2],
        }
        let mut other = Other {
            base: BaseOut {
                s_type: 1000202000,
                p_next: std::ptr::null_mut(),
            },
            bools: [1; 2],
        };
        let mut a = As {
            base: BaseOut {
                s_type: 1000150013,
                p_next: &mut other.base,
            },
            bools: [1; 5],
        };
        unsafe { clear_features(&mut a as *mut As as *mut c_void) };
        assert_eq!(a.bools, [0; 5]);
        assert_eq!(other.bools, [1; 2]);
    }

    #[test]
    fn a_device_asking_for_a_hidden_extension_is_caught() {
        let list = [c"VK_KHR_swapchain".as_ptr(), c"VK_NV_optical_flow".as_ptr()];
        let mut ci = DeviceCreateInfo {
            s_type: 3,
            p_next: std::ptr::null(),
            flags: 0,
            queue_create_info_count: 0,
            p_queue_create_infos: std::ptr::null(),
            enabled_layer_count: 0,
            pp_enabled_layer_names: std::ptr::null(),
            enabled_extension_count: 2,
            pp_enabled_extension_names: list.as_ptr(),
            p_enabled_features: std::ptr::null(),
        };
        assert!(unsafe { asks_hidden(&ci) });
        ci.enabled_extension_count = 1;
        assert!(!unsafe { asks_hidden(&ci) });
    }
}
