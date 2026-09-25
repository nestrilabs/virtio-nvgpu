// The IOCTL2 schema tables, for the backend's interpreter (device/src/xfer.rs).
// The guest's copy is driver/gen/nvgpu_schema.h; both are generated from
// gen/schema/*.py by gen/schema_gen.py. DO NOT EDIT -- regenerate.

use super::*;
use crate::version::DriverVersion;

static DRM_FIELDS: &[Field] = &[
    // 0 GETRESOURCES.fb_id_ptr
    Field { name: "fb_id_ptr", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 32, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 32, width: 4, elem: 4 }, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 1 GETRESOURCES.crtc_id_ptr
    Field { name: "crtc_id_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 36, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 36, width: 4, elem: 4 }, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 2 GETRESOURCES.connector_id_ptr
    Field { name: "connector_id_ptr", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 40, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 40, width: 4, elem: 4 }, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 3 GETRESOURCES.encoder_id_ptr
    Field { name: "encoder_id_ptr", off: 24, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 44, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 44, width: 4, elem: 4 }, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 4 GETCRTC.set_connectors_ptr
    Field { name: "set_connectors_ptr", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(0), copyback: CopyBack::None, max: 0, stride: 0, children: Span { first: 0, len: 0 } } },
    // 5 SETCRTC.set_connectors_ptr
    Field { name: "set_connectors_ptr", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 4, elem: 4 }, copyback: CopyBack::None, max: 4096, stride: 0, children: Span { first: 0, len: 0 } } },
    // 6 CURSOR.handle
    Field { name: "handle", off: 24, cond: Some(Cond { off: 0, mask: 0x1, value: 0x1 }), kind: Kind::GemIn { validate_nvkms: true } },
    // 7 CURSOR2.handle
    Field { name: "handle", off: 24, cond: Some(Cond { off: 0, mask: 0x1, value: 0x1 }), kind: Kind::GemIn { validate_nvkms: true } },
    // 8 GETGAMMA.red
    Field { name: "red", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 4, width: 4, elem: 2 }, copyback: CopyBack::Full, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 9 GETGAMMA.green
    Field { name: "green", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 4, width: 4, elem: 2 }, copyback: CopyBack::Full, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 10 GETGAMMA.blue
    Field { name: "blue", off: 24, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 4, width: 4, elem: 2 }, copyback: CopyBack::Full, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 11 SETGAMMA.red
    Field { name: "red", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 4, width: 4, elem: 2 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 12 SETGAMMA.green
    Field { name: "green", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 4, width: 4, elem: 2 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 13 SETGAMMA.blue
    Field { name: "blue", off: 24, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 4, width: 4, elem: 2 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 14 GETCONNECTOR.encoders_ptr
    Field { name: "encoders_ptr", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 40, width: 4, elem: 4 }, copyback: CopyBack::AllOrNothing { off: 40, width: 4, elem: 4 }, max: 1024, stride: 0, children: Span { first: 0, len: 0 } } },
    // 15 GETCONNECTOR.modes_ptr
    Field { name: "modes_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 32, width: 4, elem: 68 }, copyback: CopyBack::AllOrNothing { off: 32, width: 4, elem: 68 }, max: 69632, stride: 68, children: Span { first: 0, len: 0 } } },
    // 16 GETCONNECTOR.props_ptr
    Field { name: "props_ptr", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 36, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 36, width: 4, elem: 4 }, max: 4096, stride: 0, children: Span { first: 0, len: 0 } } },
    // 17 GETCONNECTOR.prop_values_ptr
    Field { name: "prop_values_ptr", off: 24, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 36, width: 4, elem: 8 }, copyback: CopyBack::Partial { off: 36, width: 4, elem: 8 }, max: 8192, stride: 0, children: Span { first: 0, len: 0 } } },
    // 18 GETPROPERTY.values_ptr
    Field { name: "values_ptr", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 56, width: 4, elem: 8 }, copyback: CopyBack::Partial { off: 56, width: 4, elem: 8 }, max: 8192, stride: 0, children: Span { first: 0, len: 0 } } },
    // 19 GETPROPERTY.enum_blob_ptr
    Field { name: "enum_blob_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::Count { off: 60, width: 4, elem: 40 }, copyback: CopyBack::Partial { off: 60, width: 4, elem: 40 }, max: 40960, stride: 40, children: Span { first: 0, len: 0 } } },
    // 20 GETPROPBLOB.data
    Field { name: "data", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 4, width: 4, elem: 1 }, copyback: CopyBack::Exact { off: 4, width: 4 }, max: 1048576, stride: 0, children: Span { first: 0, len: 0 } } },
    // 21 GETFB.handle
    Field { name: "handle", off: 24, cond: None, kind: Kind::GemOut },
    // 22 GETFB2.handles
    Field { name: "handles", off: 20, cond: None, kind: Kind::Array { count: 4, stride: 4, children: Span { first: 23, len: 1 } } },
    // 23 GETFB2.handles.[]
    Field { name: "", off: 0, cond: None, kind: Kind::GemOut },
    // 24 ADDFB.handle
    Field { name: "handle", off: 24, cond: None, kind: Kind::GemIn { validate_nvkms: true } },
    // 25 ADDFB2.handles
    Field { name: "handles", off: 20, cond: None, kind: Kind::Array { count: 4, stride: 4, children: Span { first: 26, len: 1 } } },
    // 26 ADDFB2.handles.[]
    Field { name: "", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: true } },
    // 27 DIRTYFB.clips_ptr
    Field { name: "clips_ptr", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 12, width: 4, elem: 8 }, copyback: CopyBack::None, max: 2048, stride: 8, children: Span { first: 0, len: 0 } } },
    // 28 CREATE_DUMB.handle
    Field { name: "handle", off: 16, cond: None, kind: Kind::GemOut },
    // 29 GETPLANERESOURCES.plane_id_ptr
    Field { name: "plane_id_ptr", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 8, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 8, width: 4, elem: 4 }, max: 4096, stride: 0, children: Span { first: 0, len: 0 } } },
    // 30 GETPLANE.format_type_ptr
    Field { name: "format_type_ptr", off: 24, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 20, width: 4, elem: 4 }, copyback: CopyBack::AllOrNothing { off: 20, width: 4, elem: 4 }, max: 4096, stride: 0, children: Span { first: 0, len: 0 } } },
    // 31 OBJ_GETPROPERTIES.props_ptr
    Field { name: "props_ptr", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 16, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 16, width: 4, elem: 4 }, max: 4096, stride: 0, children: Span { first: 0, len: 0 } } },
    // 32 OBJ_GETPROPERTIES.prop_values_ptr
    Field { name: "prop_values_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 16, width: 4, elem: 8 }, copyback: CopyBack::Partial { off: 16, width: 4, elem: 8 }, max: 8192, stride: 0, children: Span { first: 0, len: 0 } } },
    // 33 ATOMIC.objs_ptr
    Field { name: "objs_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 4, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 34 ATOMIC.count_props_ptr
    Field { name: "count_props_ptr", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 4, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 35 ATOMIC.props_ptr
    Field { name: "props_ptr", off: 24, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Sum { field: 34, elem: 4 }, copyback: CopyBack::None, max: 262144, stride: 0, children: Span { first: 0, len: 0 } } },
    // 36 ATOMIC.prop_values_ptr
    Field { name: "prop_values_ptr", off: 32, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Sum { field: 34, elem: 8 }, copyback: CopyBack::None, max: 524288, stride: 0, children: Span { first: 0, len: 0 } } },
    // 37 CREATEPROPBLOB.data
    Field { name: "data", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 4, elem: 1 }, copyback: CopyBack::None, max: 1048576, stride: 0, children: Span { first: 0, len: 0 } } },
    // 38 CREATE_LEASE.object_ids
    Field { name: "object_ids", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 39 CREATE_LEASE.fd
    Field { name: "fd", off: 20, cond: None, kind: Kind::FdOut { width: 4 } },
    // 40 LIST_LESSEES.lessees_ptr
    Field { name: "lessees_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 0, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 0, width: 4, elem: 4 }, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 41 GET_LEASE.objects_ptr
    Field { name: "objects_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 0, width: 4, elem: 4 }, copyback: CopyBack::Partial { off: 0, width: 4, elem: 4 }, max: 65536, stride: 0, children: Span { first: 0, len: 0 } } },
    // 42 NV_GRANT_PERMISSIONS.fd
    Field { name: "fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 43 SYNCOBJ_HANDLE_TO_FD.fd
    Field { name: "fd", off: 8, cond: None, kind: Kind::FdOut { width: 4 } },
    // 44 SYNCOBJ_FD_TO_HANDLE.fd
    Field { name: "fd", off: 8, cond: Some(Cond { off: 4, mask: 0x1, value: 0x1 }), kind: Kind::FdIn { width: 4, kinds: 0x20, none: -1 } },
    // 45 SYNCOBJ_FD_TO_HANDLE.fd
    Field { name: "fd", off: 8, cond: Some(Cond { off: 4, mask: 0x1, value: 0x0 }), kind: Kind::FdIn { width: 4, kinds: 0x40, none: -1 } },
    // 46 SYNCOBJ_WAIT.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 47 SYNCOBJ_RESET.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 48 SYNCOBJ_SIGNAL.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 49 SYNCOBJ_TIMELINE_WAIT.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 24, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 50 SYNCOBJ_TIMELINE_WAIT.points
    Field { name: "points", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 24, width: 4, elem: 8 }, copyback: CopyBack::None, max: 32768, stride: 0, children: Span { first: 0, len: 0 } } },
    // 51 SYNCOBJ_QUERY.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 52 SYNCOBJ_QUERY.points
    Field { name: "points", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 16, width: 4, elem: 8 }, copyback: CopyBack::Full, max: 32768, stride: 0, children: Span { first: 0, len: 0 } } },
    // 53 SYNCOBJ_TIMELINE_SIGNAL.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 54 SYNCOBJ_TIMELINE_SIGNAL.points
    Field { name: "points", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 8 }, copyback: CopyBack::None, max: 32768, stride: 0, children: Span { first: 0, len: 0 } } },
    // 55 SYNCOBJ_EVENTFD.fd
    Field { name: "fd", off: 16, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x100, none: -1 } },
    // 56 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 28, stride: 28, children: Span { first: 58, len: 1 } } },
    // 57 NV_GEM_IMPORT_NVKMS_MEMORY.handle
    Field { name: "handle", off: 24, cond: None, kind: Kind::GemOut },
    // 58 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd
    Field { name: "memFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10000, none: -1 } },
    // 59 NV_GEM_EXPORT_NVKMS_MEMORY.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 60 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 4, stride: 4, children: Span { first: 61, len: 1 } } },
    // 61 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd
    Field { name: "memFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10000, none: -1 } },
    // 62 NV_GEM_ALLOC_NVKMS_MEMORY.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemOut },
    // 63 NV_GEM_EXPORT_DMABUF_MEMORY.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 64 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 4, stride: 4, children: Span { first: 65, len: 1 } } },
    // 65 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr.memFd
    Field { name: "memFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10000, none: -1 } },
    // 66 NV_SEMSURF_FENCE_CTX_CREATE.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 67 NV_SEMSURF_FENCE_CTX_CREATE.handle
    Field { name: "handle", off: 24, cond: None, kind: Kind::GemOut },
    // 68 NV_SEMSURF_FENCE_CREATE.fence_context_handle
    Field { name: "fence_context_handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 69 NV_SEMSURF_FENCE_CREATE.fd
    Field { name: "fd", off: 16, cond: None, kind: Kind::FdOut { width: 4 } },
    // 70 NV_SEMSURF_FENCE_WAIT.fence_context_handle
    Field { name: "fence_context_handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 71 NV_SEMSURF_FENCE_WAIT.fd
    Field { name: "fd", off: 4, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20, none: -1 } },
    // 72 NV_SEMSURF_FENCE_ATTACH.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 73 NV_SEMSURF_FENCE_ATTACH.fence_context_handle
    Field { name: "fence_context_handle", off: 4, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
];

static DRM_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "GET_CAP", class: Class::Kms, cmd: 0xc010640c, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "SET_CLIENT_CAP", class: Class::Kms, cmd: 0x4010640d, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "WAIT_VBLANK", class: Class::Kms, cmd: 0xc018643a, nvkms_cmd: 0, size: 24, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "CRTC_GET_SEQUENCE", class: Class::Kms, cmd: 0xc018643b, nvkms_cmd: 0, size: 24, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "CRTC_QUEUE_SEQUENCE", class: Class::Kms, cmd: 0xc018643c, nvkms_cmd: 0, size: 24, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "GETRESOURCES", class: Class::Kms, cmd: 0xc04064a0, nvkms_cmd: 0, size: 64, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 4 } },
    Ioctl { name: "GETCRTC", class: Class::Kms, cmd: 0xc06864a1, nvkms_cmd: 0, size: 104, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 4, len: 1 } },
    Ioctl { name: "SETCRTC", class: Class::Kms, cmd: 0xc06864a2, nvkms_cmd: 0, size: 104, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 5, len: 1 } },
    Ioctl { name: "CURSOR", class: Class::Kms, cmd: 0xc01c64a3, nvkms_cmd: 0, size: 28, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 6, len: 1 } },
    Ioctl { name: "CURSOR2", class: Class::Kms, cmd: 0xc02464bb, nvkms_cmd: 0, size: 36, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 7, len: 1 } },
    Ioctl { name: "GETGAMMA", class: Class::Kms, cmd: 0xc02064a4, nvkms_cmd: 0, size: 32, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 8, len: 3 } },
    Ioctl { name: "SETGAMMA", class: Class::Kms, cmd: 0xc02064a5, nvkms_cmd: 0, size: 32, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 11, len: 3 } },
    Ioctl { name: "GETENCODER", class: Class::Kms, cmd: 0xc01464a6, nvkms_cmd: 0, size: 20, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 14, len: 0 } },
    Ioctl { name: "GETCONNECTOR", class: Class::Kms, cmd: 0xc05064a7, nvkms_cmd: 0, size: 80, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 14, len: 4 } },
    Ioctl { name: "GETPROPERTY", class: Class::Kms, cmd: 0xc04064aa, nvkms_cmd: 0, size: 64, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 18, len: 2 } },
    Ioctl { name: "SETPROPERTY", class: Class::Kms, cmd: 0xc01064ab, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x40, arg_in_only: false, fields: Span { first: 20, len: 0 } },
    Ioctl { name: "GETPROPBLOB", class: Class::Kms, cmd: 0xc01064ac, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 20, len: 1 } },
    Ioctl { name: "GETFB", class: Class::Kms, cmd: 0xc01c64ad, nvkms_cmd: 0, size: 28, exec: Exec::Executor, special: Special::None, policy: 0x20, arg_in_only: false, fields: Span { first: 21, len: 1 } },
    Ioctl { name: "GETFB2", class: Class::Kms, cmd: 0xc06864ce, nvkms_cmd: 0, size: 104, exec: Exec::Executor, special: Special::None, policy: 0x20, arg_in_only: false, fields: Span { first: 22, len: 1 } },
    Ioctl { name: "ADDFB", class: Class::Kms, cmd: 0xc01c64ae, nvkms_cmd: 0, size: 28, exec: Exec::Executor, special: Special::None, policy: 0x8, arg_in_only: false, fields: Span { first: 24, len: 1 } },
    Ioctl { name: "ADDFB2", class: Class::Kms, cmd: 0xc06864b8, nvkms_cmd: 0, size: 104, exec: Exec::Executor, special: Special::None, policy: 0x88, arg_in_only: false, fields: Span { first: 25, len: 1 } },
    Ioctl { name: "RMFB", class: Class::Kms, cmd: 0xc00464af, nvkms_cmd: 0, size: 4, exec: Exec::Executor, special: Special::None, policy: 0x10, arg_in_only: false, fields: Span { first: 27, len: 0 } },
    Ioctl { name: "CLOSEFB", class: Class::Kms, cmd: 0xc00864d0, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x10, arg_in_only: false, fields: Span { first: 27, len: 0 } },
    Ioctl { name: "PAGE_FLIP", class: Class::Kms, cmd: 0xc01864b0, nvkms_cmd: 0, size: 24, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 27, len: 0 } },
    Ioctl { name: "DIRTYFB", class: Class::Kms, cmd: 0xc01864b1, nvkms_cmd: 0, size: 24, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 27, len: 1 } },
    Ioctl { name: "CREATE_DUMB", class: Class::Kms, cmd: 0xc02064b2, nvkms_cmd: 0, size: 32, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 28, len: 1 } },
    Ioctl { name: "GETPLANERESOURCES", class: Class::Kms, cmd: 0xc01064b5, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 29, len: 1 } },
    Ioctl { name: "GETPLANE", class: Class::Kms, cmd: 0xc02064b6, nvkms_cmd: 0, size: 32, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 30, len: 1 } },
    Ioctl { name: "SETPLANE", class: Class::Kms, cmd: 0xc03064b7, nvkms_cmd: 0, size: 48, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 31, len: 0 } },
    Ioctl { name: "OBJ_GETPROPERTIES", class: Class::Kms, cmd: 0xc02064b9, nvkms_cmd: 0, size: 32, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 31, len: 2 } },
    Ioctl { name: "OBJ_SETPROPERTY", class: Class::Kms, cmd: 0xc01864ba, nvkms_cmd: 0, size: 24, exec: Exec::Executor, special: Special::None, policy: 0x40, arg_in_only: false, fields: Span { first: 33, len: 0 } },
    Ioctl { name: "ATOMIC", class: Class::Kms, cmd: 0xc03864bc, nvkms_cmd: 0, size: 56, exec: Exec::Executor, special: Special::Atomic, policy: 0x0, arg_in_only: false, fields: Span { first: 33, len: 4 } },
    Ioctl { name: "CREATEPROPBLOB", class: Class::Kms, cmd: 0xc01064bd, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 37, len: 1 } },
    Ioctl { name: "DESTROYPROPBLOB", class: Class::Kms, cmd: 0xc00464be, nvkms_cmd: 0, size: 4, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 38, len: 0 } },
    Ioctl { name: "CREATE_LEASE", class: Class::Kms, cmd: 0xc01864c6, nvkms_cmd: 0, size: 24, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 38, len: 2 } },
    Ioctl { name: "LIST_LESSEES", class: Class::Kms, cmd: 0xc01064c7, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 40, len: 1 } },
    Ioctl { name: "GET_LEASE", class: Class::Kms, cmd: 0xc01064c8, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 41, len: 1 } },
    Ioctl { name: "REVOKE_LEASE", class: Class::Kms, cmd: 0xc00464c9, nvkms_cmd: 0, size: 4, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 42, len: 0 } },
    Ioctl { name: "NV_GET_CRTC_CRC32", class: Class::Kms, cmd: 0xc0086440, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 42, len: 0 } },
    Ioctl { name: "NV_GET_CRTC_CRC32_V2", class: Class::Kms, cmd: 0xc01c644c, nvkms_cmd: 0, size: 28, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 42, len: 0 } },
    Ioctl { name: "NV_GET_DPY_ID_FOR_CONNECTOR_ID", class: Class::Kms, cmd: 0xc0086450, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 42, len: 0 } },
    Ioctl { name: "NV_GET_CONNECTOR_ID_FOR_DPY_ID", class: Class::Kms, cmd: 0xc0086451, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 42, len: 0 } },
    Ioctl { name: "NV_GET_CLIENT_CAPABILITY", class: Class::Kms, cmd: 0xc0106448, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 42, len: 0 } },
    Ioctl { name: "NV_GRANT_PERMISSIONS", class: Class::Kms, cmd: 0xc00c6452, nvkms_cmd: 0, size: 12, exec: Exec::Executor, special: Special::None, policy: 0x2, arg_in_only: false, fields: Span { first: 42, len: 1 } },
    Ioctl { name: "NV_REVOKE_PERMISSIONS", class: Class::Kms, cmd: 0xc0086453, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x4, arg_in_only: false, fields: Span { first: 43, len: 0 } },
    Ioctl { name: "SYNCOBJ_CREATE", class: Class::Render, cmd: 0xc00864bf, nvkms_cmd: 0, size: 8, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 43, len: 0 } },
    Ioctl { name: "SYNCOBJ_DESTROY", class: Class::Render, cmd: 0xc00864c0, nvkms_cmd: 0, size: 8, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 43, len: 0 } },
    Ioctl { name: "SYNCOBJ_HANDLE_TO_FD", class: Class::Render, cmd: 0xc01864c1, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "SYNCOBJ_FD_TO_HANDLE", class: Class::Render, cmd: 0xc01864c2, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 44, len: 2 } },
    Ioctl { name: "SYNCOBJ_WAIT", class: Class::Render, cmd: 0xc02864c3, nvkms_cmd: 0, size: 40, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 46, len: 1 } },
    Ioctl { name: "SYNCOBJ_RESET", class: Class::Render, cmd: 0xc01064c4, nvkms_cmd: 0, size: 16, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 47, len: 1 } },
    Ioctl { name: "SYNCOBJ_SIGNAL", class: Class::Render, cmd: 0xc01064c5, nvkms_cmd: 0, size: 16, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "SYNCOBJ_TIMELINE_WAIT", class: Class::Render, cmd: 0xc03064ca, nvkms_cmd: 0, size: 48, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 49, len: 2 } },
    Ioctl { name: "SYNCOBJ_QUERY", class: Class::Render, cmd: 0xc01864cb, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 51, len: 2 } },
    Ioctl { name: "SYNCOBJ_TRANSFER", class: Class::Render, cmd: 0xc02064cc, nvkms_cmd: 0, size: 32, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 53, len: 0 } },
    Ioctl { name: "SYNCOBJ_TIMELINE_SIGNAL", class: Class::Render, cmd: 0xc01864cd, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 53, len: 2 } },
    Ioctl { name: "SYNCOBJ_EVENTFD", class: Class::Render, cmd: 0xc01864cf, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 55, len: 1 } },
    Ioctl { name: "NV_GET_CRTC_CRC32", class: Class::Render, cmd: 0xc0086440, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 56, len: 0 } },
    Ioctl { name: "NV_GET_CRTC_CRC32_V2", class: Class::Render, cmd: 0xc01c644c, nvkms_cmd: 0, size: 28, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 56, len: 0 } },
    Ioctl { name: "NV_GET_DPY_ID_FOR_CONNECTOR_ID", class: Class::Render, cmd: 0xc0086450, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 56, len: 0 } },
    Ioctl { name: "NV_GET_CONNECTOR_ID_FOR_DPY_ID", class: Class::Render, cmd: 0xc0086451, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 56, len: 0 } },
    Ioctl { name: "NV_GEM_IMPORT_NVKMS_MEMORY", class: Class::Render, cmd: 0xc0206441, nvkms_cmd: 0, size: 32, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 56, len: 2 } },
    Ioctl { name: "NV_GET_DEV_INFO", class: Class::Render, cmd: 0xc0246443, nvkms_cmd: 0, size: 36, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 59, len: 0 } },
    Ioctl { name: "NV_FENCE_SUPPORTED", class: Class::Render, cmd: 0x00006444, nvkms_cmd: 0, size: 0, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 59, len: 0 } },
    Ioctl { name: "NV_GEM_EXPORT_NVKMS_MEMORY", class: Class::Render, cmd: 0xc0186449, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 59, len: 2 } },
    Ioctl { name: "NV_GEM_ALLOC_NVKMS_MEMORY", class: Class::Render, cmd: 0xc018644b, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 62, len: 1 } },
    Ioctl { name: "NV_GEM_EXPORT_DMABUF_MEMORY", class: Class::Render, cmd: 0xc018644d, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 63, len: 2 } },
    Ioctl { name: "NV_DMABUF_SUPPORTED", class: Class::Render, cmd: 0x0000644f, nvkms_cmd: 0, size: 0, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 66, len: 0 } },
    Ioctl { name: "NV_SEMSURF_FENCE_CTX_CREATE", class: Class::Render, cmd: 0xc0206454, nvkms_cmd: 0, size: 32, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 66, len: 2 } },
    Ioctl { name: "NV_SEMSURF_FENCE_CREATE", class: Class::Render, cmd: 0xc0186455, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 68, len: 2 } },
    Ioctl { name: "NV_SEMSURF_FENCE_WAIT", class: Class::Render, cmd: 0x40186456, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 70, len: 2 } },
    Ioctl { name: "NV_SEMSURF_FENCE_ATTACH", class: Class::Render, cmd: 0x40186457, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 72, len: 2 } },
];

static DRM: Table = Table {
    name: "drm",
    versions: None,
    ioctls: DRM_IOCTLS,
    fields: DRM_FIELDS,
};

static V610_57_04_EXAMPLE_FIELDS: &[Field] = &[
    // 0 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 32 }, max: 44, stride: 44, children: Span { first: 0, len: 0 } } },
];

static V610_57_04_EXAMPLE_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 3, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 0, len: 1 } },
];

static V610_57_04_EXAMPLE: Table = Table {
    name: "v610_57_04_example",
    versions: Some((DriverVersion::new(610, 57, 4), DriverVersion::new(610, 57, 4))),
    ioctls: V610_57_04_EXAMPLE_IOCTLS,
    fields: V610_57_04_EXAMPLE_FIELDS,
};

/// DRM core and nvidia-drm entries (classes Render and Kms), for any host version.
pub static DRM_TABLE: &Table = &DRM;

/// NVKMS entries, one table per range of host driver versions.
pub static MODESET_TABLES: &[&Table] = &[&V610_57_04_EXAMPLE];

/// (fourcc, planes) of every multi-planar format; the rest have one plane.
pub static MULTI_PLANE_FORMATS: &[(u32, u8)] = &[
    (0x38413542, 2), // DRM_FORMAT_BGR565_A8
    (0x38413842, 2), // DRM_FORMAT_BGR888_A8
    (0x38415842, 2), // DRM_FORMAT_BGRX8888_A8
    (0x3231564e, 2), // DRM_FORMAT_NV12
    (0x3531564e, 2), // DRM_FORMAT_NV15
    (0x3631564e, 2), // DRM_FORMAT_NV16
    (0x3032564e, 2), // DRM_FORMAT_NV20
    (0x3132564e, 2), // DRM_FORMAT_NV21
    (0x3432564e, 2), // DRM_FORMAT_NV24
    (0x3033564e, 2), // DRM_FORMAT_NV30
    (0x3234564e, 2), // DRM_FORMAT_NV42
    (0x3136564e, 2), // DRM_FORMAT_NV61
    (0x30313050, 2), // DRM_FORMAT_P010
    (0x32313050, 2), // DRM_FORMAT_P012
    (0x36313050, 2), // DRM_FORMAT_P016
    (0x30333050, 2), // DRM_FORMAT_P030
    (0x30313250, 2), // DRM_FORMAT_P210
    (0x30333250, 2), // DRM_FORMAT_P230
    (0x31303451, 3), // DRM_FORMAT_Q401
    (0x30313451, 3), // DRM_FORMAT_Q410
    (0x38413552, 2), // DRM_FORMAT_RGB565_A8
    (0x38413852, 2), // DRM_FORMAT_RGB888_A8
    (0x38415852, 2), // DRM_FORMAT_RGBX8888_A8
    (0x30313053, 3), // DRM_FORMAT_S010
    (0x32313053, 3), // DRM_FORMAT_S012
    (0x36313053, 3), // DRM_FORMAT_S016
    (0x30313253, 3), // DRM_FORMAT_S210
    (0x32313253, 3), // DRM_FORMAT_S212
    (0x36313253, 3), // DRM_FORMAT_S216
    (0x30313453, 3), // DRM_FORMAT_S410
    (0x32313453, 3), // DRM_FORMAT_S412
    (0x36313453, 3), // DRM_FORMAT_S416
    (0x30333454, 3), // DRM_FORMAT_T430
    (0x38414258, 2), // DRM_FORMAT_XBGR8888_A8
    (0x38415258, 2), // DRM_FORMAT_XRGB8888_A8
    (0x39565559, 3), // DRM_FORMAT_YUV410
    (0x31315559, 3), // DRM_FORMAT_YUV411
    (0x32315559, 3), // DRM_FORMAT_YUV420
    (0x36315559, 3), // DRM_FORMAT_YUV422
    (0x34325559, 3), // DRM_FORMAT_YUV444
    (0x39555659, 3), // DRM_FORMAT_YVU410
    (0x31315659, 3), // DRM_FORMAT_YVU411
    (0x32315659, 3), // DRM_FORMAT_YVU420
    (0x36315659, 3), // DRM_FORMAT_YVU422
    (0x34325659, 3), // DRM_FORMAT_YVU444
];
