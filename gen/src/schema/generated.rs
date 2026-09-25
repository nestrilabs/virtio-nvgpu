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
    Field { name: "handle", off: 24, cond: Some(Cond { off: 0, mask: 0x1, value: 0x1, ne: false }), kind: Kind::GemIn { validate_nvkms: true } },
    // 7 CURSOR2.handle
    Field { name: "handle", off: 24, cond: Some(Cond { off: 0, mask: 0x1, value: 0x1, ne: false }), kind: Kind::GemIn { validate_nvkms: true } },
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
    Field { name: "handles", off: 20, cond: None, kind: Kind::Array { count: 4, stride: 4, limit: Limit::All, children: Span { first: 23, len: 1 } } },
    // 23 GETFB2.handles.[]
    Field { name: "", off: 0, cond: None, kind: Kind::GemOut },
    // 24 ADDFB.handle
    Field { name: "handle", off: 24, cond: None, kind: Kind::GemIn { validate_nvkms: true } },
    // 25 ADDFB2.handles
    Field { name: "handles", off: 20, cond: None, kind: Kind::Array { count: 4, stride: 4, limit: Limit::All, children: Span { first: 26, len: 1 } } },
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
    // 43 NV_GRANT_PERMISSIONS_UNTYPED.fd
    Field { name: "fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 44 SYNCOBJ_HANDLE_TO_FD.fd
    Field { name: "fd", off: 8, cond: None, kind: Kind::FdOut { width: 4 } },
    // 45 SYNCOBJ_FD_TO_HANDLE.fd
    Field { name: "fd", off: 8, cond: Some(Cond { off: 4, mask: 0x1, value: 0x1, ne: false }), kind: Kind::FdIn { width: 4, kinds: 0x20, none: -1 } },
    // 46 SYNCOBJ_FD_TO_HANDLE.fd
    Field { name: "fd", off: 8, cond: Some(Cond { off: 4, mask: 0x1, value: 0x0, ne: false }), kind: Kind::FdIn { width: 4, kinds: 0x40, none: -1 } },
    // 47 SYNCOBJ_WAIT.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 48 SYNCOBJ_RESET.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 49 SYNCOBJ_SIGNAL.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 50 SYNCOBJ_TIMELINE_WAIT.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 24, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 51 SYNCOBJ_TIMELINE_WAIT.points
    Field { name: "points", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 24, width: 4, elem: 8 }, copyback: CopyBack::None, max: 32768, stride: 0, children: Span { first: 0, len: 0 } } },
    // 52 SYNCOBJ_QUERY.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 53 SYNCOBJ_QUERY.points
    Field { name: "points", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 16, width: 4, elem: 8 }, copyback: CopyBack::Full, max: 32768, stride: 0, children: Span { first: 0, len: 0 } } },
    // 54 SYNCOBJ_TIMELINE_SIGNAL.handles
    Field { name: "handles", off: 0, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4 }, copyback: CopyBack::None, max: 16384, stride: 0, children: Span { first: 0, len: 0 } } },
    // 55 SYNCOBJ_TIMELINE_SIGNAL.points
    Field { name: "points", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 8 }, copyback: CopyBack::None, max: 32768, stride: 0, children: Span { first: 0, len: 0 } } },
    // 56 SYNCOBJ_EVENTFD.fd
    Field { name: "fd", off: 16, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x100, none: -1 } },
    // 57 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 28, stride: 28, children: Span { first: 59, len: 1 } } },
    // 58 NV_GEM_IMPORT_NVKMS_MEMORY.handle
    Field { name: "handle", off: 24, cond: None, kind: Kind::GemOut },
    // 59 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd
    Field { name: "memFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10000, none: -1 } },
    // 60 NV_GEM_EXPORT_NVKMS_MEMORY.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 61 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 4, stride: 4, children: Span { first: 62, len: 1 } } },
    // 62 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd
    Field { name: "memFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10000, none: -1 } },
    // 63 NV_GEM_ALLOC_NVKMS_MEMORY.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemOut },
    // 64 NV_GEM_EXPORT_DMABUF_MEMORY.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 65 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 4, stride: 4, children: Span { first: 66, len: 1 } } },
    // 66 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr.memFd
    Field { name: "memFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10000, none: -1 } },
    // 67 NV_SEMSURF_FENCE_CTX_CREATE.nvkms_params_ptr
    Field { name: "nvkms_params_ptr", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 8, elem: 1 }, copyback: CopyBack::None, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 68 NV_SEMSURF_FENCE_CTX_CREATE.handle
    Field { name: "handle", off: 24, cond: None, kind: Kind::GemOut },
    // 69 NV_SEMSURF_FENCE_CREATE.fence_context_handle
    Field { name: "fence_context_handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 70 NV_SEMSURF_FENCE_CREATE.fd
    Field { name: "fd", off: 16, cond: None, kind: Kind::FdOut { width: 4 } },
    // 71 NV_SEMSURF_FENCE_WAIT.fence_context_handle
    Field { name: "fence_context_handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 72 NV_SEMSURF_FENCE_WAIT.fd
    Field { name: "fd", off: 4, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20, none: -1 } },
    // 73 NV_SEMSURF_FENCE_ATTACH.handle
    Field { name: "handle", off: 0, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
    // 74 NV_SEMSURF_FENCE_ATTACH.fence_context_handle
    Field { name: "fence_context_handle", off: 4, cond: None, kind: Kind::GemIn { validate_nvkms: false } },
];

static DRM_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "GET_CAP", class: Class::Kms, cmd: 0xc010640c, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "SET_CLIENT_CAP", class: Class::Kms, cmd: 0x4010640d, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "SET_MASTER", class: Class::Kms, cmd: 0x0000641e, nvkms_cmd: 0, size: 0, exec: Exec::Executor, special: Special::None, policy: 0x400, arg_in_only: false, fields: Span { first: 0, len: 0 } },
    Ioctl { name: "DROP_MASTER", class: Class::Kms, cmd: 0x0000641f, nvkms_cmd: 0, size: 0, exec: Exec::Executor, special: Special::None, policy: 0x400, arg_in_only: false, fields: Span { first: 0, len: 0 } },
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
    Ioctl { name: "NV_GRANT_PERMISSIONS_UNTYPED", class: Class::Kms, cmd: 0xc0086452, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x2, arg_in_only: false, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "NV_REVOKE_PERMISSIONS_UNTYPED", class: Class::Kms, cmd: 0xc0046453, nvkms_cmd: 0, size: 4, exec: Exec::Executor, special: Special::None, policy: 0x4, arg_in_only: false, fields: Span { first: 44, len: 0 } },
    Ioctl { name: "SYNCOBJ_CREATE", class: Class::Render, cmd: 0xc00864bf, nvkms_cmd: 0, size: 8, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 44, len: 0 } },
    Ioctl { name: "SYNCOBJ_DESTROY", class: Class::Render, cmd: 0xc00864c0, nvkms_cmd: 0, size: 8, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 44, len: 0 } },
    Ioctl { name: "SYNCOBJ_HANDLE_TO_FD", class: Class::Render, cmd: 0xc01864c1, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 44, len: 1 } },
    Ioctl { name: "SYNCOBJ_FD_TO_HANDLE", class: Class::Render, cmd: 0xc01864c2, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 45, len: 2 } },
    Ioctl { name: "SYNCOBJ_WAIT", class: Class::Render, cmd: 0xc02864c3, nvkms_cmd: 0, size: 40, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 47, len: 1 } },
    Ioctl { name: "SYNCOBJ_RESET", class: Class::Render, cmd: 0xc01064c4, nvkms_cmd: 0, size: 16, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "SYNCOBJ_SIGNAL", class: Class::Render, cmd: 0xc01064c5, nvkms_cmd: 0, size: 16, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 49, len: 1 } },
    Ioctl { name: "SYNCOBJ_TIMELINE_WAIT", class: Class::Render, cmd: 0xc03064ca, nvkms_cmd: 0, size: 48, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 50, len: 2 } },
    Ioctl { name: "SYNCOBJ_QUERY", class: Class::Render, cmd: 0xc01864cb, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 52, len: 2 } },
    Ioctl { name: "SYNCOBJ_TRANSFER", class: Class::Render, cmd: 0xc02064cc, nvkms_cmd: 0, size: 32, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 54, len: 0 } },
    Ioctl { name: "SYNCOBJ_TIMELINE_SIGNAL", class: Class::Render, cmd: 0xc01864cd, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 54, len: 2 } },
    Ioctl { name: "SYNCOBJ_EVENTFD", class: Class::Render, cmd: 0xc01864cf, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 56, len: 1 } },
    Ioctl { name: "NV_GET_CRTC_CRC32", class: Class::Render, cmd: 0xc0086440, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 57, len: 0 } },
    Ioctl { name: "NV_GET_CRTC_CRC32_V2", class: Class::Render, cmd: 0xc01c644c, nvkms_cmd: 0, size: 28, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 57, len: 0 } },
    Ioctl { name: "NV_GET_DPY_ID_FOR_CONNECTOR_ID", class: Class::Render, cmd: 0xc0086450, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 57, len: 0 } },
    Ioctl { name: "NV_GET_CONNECTOR_ID_FOR_DPY_ID", class: Class::Render, cmd: 0xc0086451, nvkms_cmd: 0, size: 8, exec: Exec::Executor, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 57, len: 0 } },
    Ioctl { name: "NV_GEM_IMPORT_NVKMS_MEMORY", class: Class::Render, cmd: 0xc0206441, nvkms_cmd: 0, size: 32, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 57, len: 2 } },
    Ioctl { name: "NV_GET_DEV_INFO", class: Class::Render, cmd: 0xc0246443, nvkms_cmd: 0, size: 36, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 60, len: 0 } },
    Ioctl { name: "NV_FENCE_SUPPORTED", class: Class::Render, cmd: 0x00006444, nvkms_cmd: 0, size: 0, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 60, len: 0 } },
    Ioctl { name: "NV_GEM_EXPORT_NVKMS_MEMORY", class: Class::Render, cmd: 0xc0186449, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 60, len: 2 } },
    Ioctl { name: "NV_GEM_ALLOC_NVKMS_MEMORY", class: Class::Render, cmd: 0xc018644b, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 63, len: 1 } },
    Ioctl { name: "NV_GEM_EXPORT_DMABUF_MEMORY", class: Class::Render, cmd: 0xc018644d, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 64, len: 2 } },
    Ioctl { name: "NV_DMABUF_SUPPORTED", class: Class::Render, cmd: 0x0000644f, nvkms_cmd: 0, size: 0, exec: Exec::Inline, special: Special::None, policy: 0x0, arg_in_only: false, fields: Span { first: 67, len: 0 } },
    Ioctl { name: "NV_SEMSURF_FENCE_CTX_CREATE", class: Class::Render, cmd: 0xc0206454, nvkms_cmd: 0, size: 32, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 67, len: 2 } },
    Ioctl { name: "NV_SEMSURF_FENCE_CREATE", class: Class::Render, cmd: 0xc0186455, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 69, len: 2 } },
    Ioctl { name: "NV_SEMSURF_FENCE_WAIT", class: Class::Render, cmd: 0x40186456, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 71, len: 2 } },
    Ioctl { name: "NV_SEMSURF_FENCE_ATTACH", class: Class::Render, cmd: 0x40186457, nvkms_cmd: 0, size: 24, exec: Exec::Inline, special: Special::None, policy: 0x1, arg_in_only: false, fields: Span { first: 73, len: 2 } },
];

static DRM: Table = Table {
    name: "drm",
    versions: None,
    ioctls: DRM_IOCTLS,
    fields: DRM_FIELDS,
    planes: &[],
    nvkms: None,
};

static V535_129_03_FIELDS: &[Field] = &[
    // 0 NVKMS_ALLOC_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 616, len: 464 }, max: 1080, stride: 1080, children: Span { first: 0, len: 0 } } },
    // 1 NVKMS_FREE_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 2 NVKMS_QUERY_DISP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 164 }, max: 172, stride: 172, children: Span { first: 0, len: 0 } } },
    // 3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 32 }, max: 44, stride: 44, children: Span { first: 0, len: 0 } } },
    // 4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 5 NVKMS_QUERY_DPY_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 80 }, max: 92, stride: 92, children: Span { first: 0, len: 0 } } },
    // 6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2072, len: 35024 }, max: 37096, stride: 37096, children: Span { first: 0, len: 0 } } },
    // 7 NVKMS_VALIDATE_MODE_INDEX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 200, len: 496 }, max: 696, stride: 696, children: Span { first: 8, len: 1 } } },
    // 8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString
    Field { name: "request.pInfoString", off: 192, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 184, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 500, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 9 NVKMS_VALIDATE_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 272, len: 352 }, max: 624, stride: 624, children: Span { first: 10, len: 1 } } },
    // 10 NVKMS_VALIDATE_MODE.address.request.pInfoString
    Field { name: "request.pInfoString", off: 264, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 256, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 424, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 11 NVKMS_SET_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 78936, len: 15944 }, max: 94880, stride: 94880, children: Span { first: 12, len: 1 } } },
    // 12 NVKMS_SET_MODE.address.request.disp
    Field { name: "request.disp", off: 16, cond: None, kind: Kind::Array { count: 8, stride: 9864, limit: Limit::All, children: Span { first: 13, len: 1 } } },
    // 13 NVKMS_SET_MODE.address.request.disp.head
    Field { name: "head", off: 8, cond: None, kind: Kind::Array { count: 4, stride: 2464, limit: Limit::All, children: Span { first: 14, len: 2 } } },
    // 14 NVKMS_SET_MODE.address.request.disp.head.lut.input.pRamps
    Field { name: "lut.input.pRamps", off: 280, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 15 NVKMS_SET_MODE.address.request.disp.head.lut.output.pRamps
    Field { name: "lut.output.pRamps", off: 296, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 16 NVKMS_SET_CURSOR_IMAGE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 48, len: 4 }, max: 52, stride: 52, children: Span { first: 0, len: 0 } } },
    // 17 NVKMS_MOVE_CURSOR.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 18 NVKMS_SET_LUT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 64, len: 4 }, max: 72, stride: 72, children: Span { first: 19, len: 2 } } },
    // 19 NVKMS_SET_LUT.address.request.common.input.pRamps
    Field { name: "request.common.input.pRamps", off: 32, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 20 NVKMS_SET_LUT.address.request.common.output.pRamps
    Field { name: "request.common.output.pRamps", off: 48, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 21 NVKMS_IDLE_BASE_CHANNEL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 16 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 22 NVKMS_FLIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 3080 }, max: 3104, stride: 3104, children: Span { first: 23, len: 1 } } },
    // 23 NVKMS_FLIP.address.request.pFlipHead
    Field { name: "request.pFlipHead", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 2056 }, copyback: CopyBack::None, max: 65792, stride: 2056, children: Span { first: 0, len: 0 } } },
    // 24 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 25 NVKMS_REGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 144, len: 4 }, max: 152, stride: 152, children: Span { first: 26, len: 1 } } },
    // 26 NVKMS_REGISTER_SURFACE.address.request.planes
    Field { name: "request.planes", off: 16, cond: Some(Cond { off: 4, mask: 0xff, value: 0x0, ne: true }), kind: Kind::Array { count: 3, stride: 32, limit: Limit::Planes { off: 124 }, children: Span { first: 27, len: 1 } } },
    // 27 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd
    Field { name: "u.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10080, none: -1 } },
    // 28 NVKMS_UNREGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 29 NVKMS_GRANT_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 30, len: 1 } } },
    // 30 NVKMS_GRANT_SURFACE.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 31 NVKMS_ACQUIRE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 32, len: 1 } } },
    // 32 NVKMS_ACQUIRE_SURFACE.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 33 NVKMS_RELEASE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 34 NVKMS_SET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 35 NVKMS_GET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 36 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 37 NVKMS_SET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 38 NVKMS_GET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 39 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 40 NVKMS_QUERY_FRAMELOCK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 16 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 41 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 42 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 8 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 43 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 24 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 44 NVKMS_GET_NEXT_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 40 }, max: 48, stride: 48, children: Span { first: 0, len: 0 } } },
    // 45 NVKMS_DECLARE_EVENT_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 46 NVKMS_CLEAR_UNICAST_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 47, len: 1 } } },
    // 47 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd
    Field { name: "request.unicastEventFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 48 NVKMS_SET_LAYER_POSITION.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1192, len: 4 }, max: 1196, stride: 1196, children: Span { first: 0, len: 0 } } },
    // 49 NVKMS_GRAB_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 50 NVKMS_RELEASE_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 51 NVKMS_GRANT_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 140, len: 4 }, max: 144, stride: 144, children: Span { first: 52, len: 1 } } },
    // 52 NVKMS_GRANT_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 53 NVKMS_ACQUIRE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 136 }, max: 140, stride: 140, children: Span { first: 54, len: 1 } } },
    // 54 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 55 NVKMS_REVOKE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 140, len: 4 }, max: 144, stride: 144, children: Span { first: 0, len: 0 } } },
    // 56 NVKMS_QUERY_DPY_CRC32.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 24 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 57 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 58 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 59 NVKMS_ALLOC_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 36, len: 4 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 60 NVKMS_FREE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 61 NVKMS_JOIN_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2564, len: 4 }, max: 2568, stride: 2568, children: Span { first: 62, len: 1 } } },
    // 62 NVKMS_JOIN_SWAP_GROUP.address.request.member
    Field { name: "request.member", off: 4, cond: None, kind: Kind::Array { count: 128, stride: 20, limit: Limit::Count { off: 0, width: 4 }, children: Span { first: 63, len: 1 } } },
    // 63 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd
    Field { name: "unicastEvent.fd", off: 12, cond: Some(Cond { off: 16, mask: 0xff, value: 0x0, ne: true }), kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 64 NVKMS_LEAVE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1028, len: 4 }, max: 1032, stride: 1032, children: Span { first: 0, len: 0 } } },
    // 65 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 66, len: 1 } } },
    // 66 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList
    Field { name: "request.pClipList", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 2, elem: 8 }, copyback: CopyBack::None, max: 524280, stride: 8, children: Span { first: 0, len: 0 } } },
    // 67 NVKMS_GRANT_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 68, len: 1 } } },
    // 68 NVKMS_GRANT_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 69 NVKMS_ACQUIRE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 70, len: 1 } } },
    // 70 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 71 NVKMS_RELEASE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 72 NVKMS_SWITCH_MUX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 73 NVKMS_GET_MUX_STATE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 74 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 75 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 76 NVKMS_NOTIFY_VBLANK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 77, len: 1 } } },
    // 77 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd
    Field { name: "request.unicastEvent.fd", off: 12, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
];

static V535_129_03_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "NVKMS_ALLOC_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 0, len: 1 } },
    Ioctl { name: "NVKMS_FREE_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 1, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 1, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DISP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 2, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 2, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 3, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 3, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 4, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 4, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 5, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 5, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 6, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 6, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE_INDEX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 7, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 7, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 8, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 9, len: 1 } },
    Ioctl { name: "NVKMS_SET_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 9, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 11, len: 1 } },
    Ioctl { name: "NVKMS_SET_CURSOR_IMAGE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 10, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 16, len: 1 } },
    Ioctl { name: "NVKMS_MOVE_CURSOR", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 11, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 17, len: 1 } },
    Ioctl { name: "NVKMS_SET_LUT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 12, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 18, len: 1 } },
    Ioctl { name: "NVKMS_IDLE_BASE_CHANNEL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 13, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 21, len: 1 } },
    Ioctl { name: "NVKMS_FLIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 14, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 22, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 15, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 24, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 16, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 25, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 17, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 28, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 18, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 29, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 19, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 31, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 20, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 33, len: 1 } },
    Ioctl { name: "NVKMS_SET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 21, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 34, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 22, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 35, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 23, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 36, len: 1 } },
    Ioctl { name: "NVKMS_SET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 24, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 37, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 25, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 38, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 26, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 39, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_FRAMELOCK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 27, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 40, len: 1 } },
    Ioctl { name: "NVKMS_SET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 28, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 41, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 29, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 42, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 30, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "NVKMS_GET_NEXT_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 31, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 44, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_EVENT_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 32, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 45, len: 1 } },
    Ioctl { name: "NVKMS_CLEAR_UNICAST_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 33, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 46, len: 1 } },
    Ioctl { name: "NVKMS_SET_LAYER_POSITION", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 36, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "NVKMS_GRAB_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 37, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 49, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 38, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 50, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 39, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 51, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 40, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 53, len: 1 } },
    Ioctl { name: "NVKMS_REVOKE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 41, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 55, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_CRC32", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 42, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 56, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 43, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 57, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 44, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 58, len: 1 } },
    Ioctl { name: "NVKMS_ALLOC_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 45, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 59, len: 1 } },
    Ioctl { name: "NVKMS_FREE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 46, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 60, len: 1 } },
    Ioctl { name: "NVKMS_JOIN_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 47, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 61, len: 1 } },
    Ioctl { name: "NVKMS_LEAVE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 48, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 64, len: 1 } },
    Ioctl { name: "NVKMS_SET_SWAP_GROUP_CLIP_LIST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 49, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 65, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 50, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 67, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 51, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 69, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 52, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 71, len: 1 } },
    Ioctl { name: "NVKMS_SWITCH_MUX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 53, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 72, len: 1 } },
    Ioctl { name: "NVKMS_GET_MUX_STATE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 54, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 73, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 56, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 74, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 57, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 75, len: 1 } },
    Ioctl { name: "NVKMS_NOTIFY_VBLANK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 58, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 76, len: 1 } },
];

static V535_129_03_LAYOUT: NvkmsLayout = NvkmsLayout {
    alloc_scrub: &[(36, 580)],
    alloc_reply_device: 620,
    alloc_reply_disps: 636,
    dpy_dynamic_scrub: &[(12, 2058)],
    set_cursor_image: NvkmsTarget { device: 0, disp: 4, what: 8 },
    move_cursor: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_lut: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_dpy_attribute: NvkmsTarget { device: 0, disp: 4, what: 8 },
    layer_position: NvkmsLayerPosition { device: 0, disps: 4, disp: Arr { off: 8, count: 8, stride: 148 }, heads: 0, head: Arr { off: 4, count: 4, stride: 36 } },
    flip: NvkmsFlipLayout { device: 0, ptr: 8, heads: 16, head_size: 2056, sd: 0, head: 4, layer: Arr { off: 72, count: 8, stride: 248 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    set_mode: NvkmsSetModeLayout { device: 0, commit: 4, disps: 8, disp: Arr { off: 16, count: 8, stride: 9864 }, heads: 0, head: Arr { off: 8, count: 4, stride: 2464 }, dpys: 0, layer: Arr { off: 376, count: 8, stride: 248 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    grant: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 4 }), head: Arr { off: 0, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 16 }), head: Arr { off: 0, count: 4, stride: 4 } } },
    acquire: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 4 }), head: Arr { off: 0, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 16 }), head: Arr { off: 0, count: 4, stride: 4 } } },
    revoke: NvkmsPerms { device: 0, ptype: 8, flip: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 4 }), head: Arr { off: 0, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 16 }), head: Arr { off: 0, count: 4, stride: 4 } } },
    event_interest: 0,
    events_allowed: 0x27,
    next_event_valid: 8,
    drm_grant_typed: false,
};

static V535_129_03: Table = Table {
    name: "v535_129_03",
    versions: Some((DriverVersion::new(535, 129, 3), DriverVersion::new(580, 178, 3))),
    ioctls: V535_129_03_IOCTLS,
    fields: V535_129_03_FIELDS,
    planes: &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1],
    nvkms: Some(&V535_129_03_LAYOUT),
};

static V580_178_04_FIELDS: &[Field] = &[
    // 0 NVKMS_ALLOC_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 624, len: 888 }, max: 1512, stride: 1512, children: Span { first: 0, len: 0 } } },
    // 1 NVKMS_FREE_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 2 NVKMS_QUERY_DISP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 164 }, max: 172, stride: 172, children: Span { first: 0, len: 0 } } },
    // 3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 32 }, max: 44, stride: 44, children: Span { first: 0, len: 0 } } },
    // 4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 5 NVKMS_QUERY_DPY_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 80 }, max: 92, stride: 92, children: Span { first: 0, len: 0 } } },
    // 6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2072, len: 35088 }, max: 37160, stride: 37160, children: Span { first: 0, len: 0 } } },
    // 7 NVKMS_VALIDATE_MODE_INDEX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 208, len: 512 }, max: 720, stride: 720, children: Span { first: 8, len: 1 } } },
    // 8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString
    Field { name: "request.pInfoString", off: 200, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 192, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 516, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 9 NVKMS_VALIDATE_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 288, len: 352 }, max: 640, stride: 640, children: Span { first: 10, len: 1 } } },
    // 10 NVKMS_VALIDATE_MODE.address.request.pInfoString
    Field { name: "request.pInfoString", off: 280, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 272, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 440, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 11 NVKMS_SET_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 155992, len: 15944 }, max: 171936, stride: 171936, children: Span { first: 12, len: 1 } } },
    // 12 NVKMS_SET_MODE.address.request.disp
    Field { name: "request.disp", off: 16, cond: None, kind: Kind::Array { count: 8, stride: 19496, limit: Limit::All, children: Span { first: 13, len: 1 } } },
    // 13 NVKMS_SET_MODE.address.request.disp.head
    Field { name: "head", off: 8, cond: None, kind: Kind::Array { count: 4, stride: 4872, limit: Limit::All, children: Span { first: 14, len: 2 } } },
    // 14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 360, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 376, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 16 NVKMS_SET_CURSOR_IMAGE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 48, len: 4 }, max: 52, stride: 52, children: Span { first: 0, len: 0 } } },
    // 17 NVKMS_MOVE_CURSOR.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 18 NVKMS_SET_LUT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 64, len: 4 }, max: 72, stride: 72, children: Span { first: 19, len: 2 } } },
    // 19 NVKMS_SET_LUT.address.request.common.input.pRamps
    Field { name: "request.common.input.pRamps", off: 32, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 20 NVKMS_SET_LUT.address.request.common.output.pRamps
    Field { name: "request.common.output.pRamps", off: 48, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 21 NVKMS_CHECK_LUT_NOTIFIER.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 1 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 22 NVKMS_IDLE_BASE_CHANNEL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 16 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 23 NVKMS_FLIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 3084 }, max: 3112, stride: 3112, children: Span { first: 24, len: 1 } } },
    // 24 NVKMS_FLIP.address.request.pFlipHead
    Field { name: "request.pFlipHead", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4488 }, copyback: CopyBack::None, max: 143616, stride: 4488, children: Span { first: 25, len: 2 } } },
    // 25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 88, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 104, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 28 NVKMS_REGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 144, len: 4 }, max: 152, stride: 152, children: Span { first: 29, len: 1 } } },
    // 29 NVKMS_REGISTER_SURFACE.address.request.planes
    Field { name: "request.planes", off: 16, cond: Some(Cond { off: 4, mask: 0xff, value: 0x0, ne: true }), kind: Kind::Array { count: 3, stride: 32, limit: Limit::Planes { off: 124 }, children: Span { first: 30, len: 1 } } },
    // 30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd
    Field { name: "u.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10080, none: -1 } },
    // 31 NVKMS_UNREGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 32 NVKMS_GRANT_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 33, len: 1 } } },
    // 33 NVKMS_GRANT_SURFACE.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 34 NVKMS_ACQUIRE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 35, len: 1 } } },
    // 35 NVKMS_ACQUIRE_SURFACE.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 36 NVKMS_RELEASE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 37 NVKMS_SET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 38 NVKMS_GET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 40 NVKMS_SET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 41 NVKMS_GET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 43 NVKMS_QUERY_FRAMELOCK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 16 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 8 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 24 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 47 NVKMS_GET_NEXT_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 40 }, max: 48, stride: 48, children: Span { first: 0, len: 0 } } },
    // 48 NVKMS_DECLARE_EVENT_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 49 NVKMS_CLEAR_UNICAST_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 50, len: 1 } } },
    // 50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd
    Field { name: "request.unicastEventFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 51 NVKMS_SET_LAYER_POSITION.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1192, len: 4 }, max: 1196, stride: 1196, children: Span { first: 0, len: 0 } } },
    // 52 NVKMS_GRAB_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 53 NVKMS_RELEASE_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 54 NVKMS_GRANT_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 140, len: 4 }, max: 144, stride: 144, children: Span { first: 55, len: 1 } } },
    // 55 NVKMS_GRANT_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 56 NVKMS_ACQUIRE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 136 }, max: 140, stride: 140, children: Span { first: 57, len: 1 } } },
    // 57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 58 NVKMS_REVOKE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 140, len: 4 }, max: 144, stride: 144, children: Span { first: 0, len: 0 } } },
    // 59 NVKMS_QUERY_DPY_CRC32.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 24 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 62 NVKMS_ALLOC_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 36, len: 4 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 63 NVKMS_FREE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 64 NVKMS_JOIN_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2564, len: 4 }, max: 2568, stride: 2568, children: Span { first: 65, len: 1 } } },
    // 65 NVKMS_JOIN_SWAP_GROUP.address.request.member
    Field { name: "request.member", off: 4, cond: None, kind: Kind::Array { count: 128, stride: 20, limit: Limit::Count { off: 0, width: 4 }, children: Span { first: 66, len: 1 } } },
    // 66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd
    Field { name: "unicastEvent.fd", off: 12, cond: Some(Cond { off: 16, mask: 0xff, value: 0x0, ne: true }), kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 67 NVKMS_LEAVE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1028, len: 4 }, max: 1032, stride: 1032, children: Span { first: 0, len: 0 } } },
    // 68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 69, len: 1 } } },
    // 69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList
    Field { name: "request.pClipList", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 2, elem: 8 }, copyback: CopyBack::None, max: 524280, stride: 8, children: Span { first: 0, len: 0 } } },
    // 70 NVKMS_GRANT_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 71, len: 1 } } },
    // 71 NVKMS_GRANT_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 72 NVKMS_ACQUIRE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 73, len: 1 } } },
    // 73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 74 NVKMS_RELEASE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 75 NVKMS_SWITCH_MUX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 76 NVKMS_GET_MUX_STATE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 79 NVKMS_NOTIFY_VBLANK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 80, len: 1 } } },
    // 80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd
    Field { name: "request.unicastEvent.fd", off: 12, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 81 NVKMS_SET_FLIPLOCK_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 324, len: 4 }, max: 328, stride: 328, children: Span { first: 0, len: 0 } } },
    // 82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
];

static V580_178_04_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "NVKMS_ALLOC_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 0, len: 1 } },
    Ioctl { name: "NVKMS_FREE_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 1, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 1, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DISP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 2, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 2, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 3, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 3, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 4, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 4, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 5, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 5, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 6, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 6, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE_INDEX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 7, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 7, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 8, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 9, len: 1 } },
    Ioctl { name: "NVKMS_SET_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 9, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 11, len: 1 } },
    Ioctl { name: "NVKMS_SET_CURSOR_IMAGE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 10, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 16, len: 1 } },
    Ioctl { name: "NVKMS_MOVE_CURSOR", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 11, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 17, len: 1 } },
    Ioctl { name: "NVKMS_SET_LUT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 12, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 18, len: 1 } },
    Ioctl { name: "NVKMS_CHECK_LUT_NOTIFIER", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 13, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 21, len: 1 } },
    Ioctl { name: "NVKMS_IDLE_BASE_CHANNEL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 14, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 22, len: 1 } },
    Ioctl { name: "NVKMS_FLIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 15, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 23, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 16, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 27, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 17, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 28, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 18, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 31, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 19, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 32, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 20, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 34, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 21, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 36, len: 1 } },
    Ioctl { name: "NVKMS_SET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 22, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 37, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 23, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 38, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 24, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 39, len: 1 } },
    Ioctl { name: "NVKMS_SET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 25, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 40, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 26, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 41, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 27, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 42, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_FRAMELOCK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 28, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "NVKMS_SET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 29, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 44, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 30, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 45, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 31, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 46, len: 1 } },
    Ioctl { name: "NVKMS_GET_NEXT_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 32, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 47, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_EVENT_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 33, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "NVKMS_CLEAR_UNICAST_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 34, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 49, len: 1 } },
    Ioctl { name: "NVKMS_SET_LAYER_POSITION", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 37, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 51, len: 1 } },
    Ioctl { name: "NVKMS_GRAB_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 38, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 52, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 39, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 53, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 40, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 54, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 41, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 56, len: 1 } },
    Ioctl { name: "NVKMS_REVOKE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 42, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 58, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_CRC32", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 43, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 59, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 44, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 60, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 45, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 61, len: 1 } },
    Ioctl { name: "NVKMS_ALLOC_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 46, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 62, len: 1 } },
    Ioctl { name: "NVKMS_FREE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 47, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 63, len: 1 } },
    Ioctl { name: "NVKMS_JOIN_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 48, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 64, len: 1 } },
    Ioctl { name: "NVKMS_LEAVE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 49, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 67, len: 1 } },
    Ioctl { name: "NVKMS_SET_SWAP_GROUP_CLIP_LIST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 50, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 68, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 51, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 70, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 52, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 72, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 53, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 74, len: 1 } },
    Ioctl { name: "NVKMS_SWITCH_MUX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 54, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 75, len: 1 } },
    Ioctl { name: "NVKMS_GET_MUX_STATE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 55, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 76, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 57, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 77, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 58, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 78, len: 1 } },
    Ioctl { name: "NVKMS_NOTIFY_VBLANK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 59, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 79, len: 1 } },
    Ioctl { name: "NVKMS_SET_FLIPLOCK_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 60, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 81, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 61, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 82, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 62, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 83, len: 1 } },
    Ioctl { name: "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 63, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 84, len: 1 } },
];

static V580_178_04_LAYOUT: NvkmsLayout = NvkmsLayout {
    alloc_scrub: &[(42, 578)],
    alloc_reply_device: 628,
    alloc_reply_disps: 644,
    dpy_dynamic_scrub: &[(12, 2058)],
    set_cursor_image: NvkmsTarget { device: 0, disp: 4, what: 8 },
    move_cursor: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_lut: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_dpy_attribute: NvkmsTarget { device: 0, disp: 4, what: 8 },
    layer_position: NvkmsLayerPosition { device: 0, disps: 4, disp: Arr { off: 8, count: 8, stride: 148 }, heads: 0, head: Arr { off: 4, count: 4, stride: 36 } },
    flip: NvkmsFlipLayout { device: 0, ptr: 8, heads: 16, head_size: 4488, sd: 0, head: 4, layer: Arr { off: 200, count: 8, stride: 536 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    set_mode: NvkmsSetModeLayout { device: 0, commit: 4, disps: 8, disp: Arr { off: 16, count: 8, stride: 19496 }, heads: 0, head: Arr { off: 8, count: 4, stride: 4872 }, dpys: 0, layer: Arr { off: 472, count: 8, stride: 536 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    grant: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 4 }), head: Arr { off: 0, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 16 }), head: Arr { off: 0, count: 4, stride: 4 } } },
    acquire: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 4 }), head: Arr { off: 0, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 16 }), head: Arr { off: 0, count: 4, stride: 4 } } },
    revoke: NvkmsPerms { device: 0, ptype: 8, flip: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 4 }), head: Arr { off: 0, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: Some(Arr { off: 12, count: 8, stride: 16 }), head: Arr { off: 0, count: 4, stride: 4 } } },
    event_interest: 0,
    events_allowed: 0x27,
    next_event_valid: 8,
    drm_grant_typed: true,
};

static V580_178_04: Table = Table {
    name: "v580_178_04",
    versions: Some((DriverVersion::new(580, 178, 4), DriverVersion::new(595, 71, 4))),
    ioctls: V580_178_04_IOCTLS,
    fields: V580_178_04_FIELDS,
    planes: &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1],
    nvkms: Some(&V580_178_04_LAYOUT),
};

static V595_71_05_FIELDS: &[Field] = &[
    // 0 NVKMS_ALLOC_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 624, len: 888 }, max: 1512, stride: 1512, children: Span { first: 0, len: 0 } } },
    // 1 NVKMS_FREE_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 2 NVKMS_QUERY_DISP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 164 }, max: 172, stride: 172, children: Span { first: 0, len: 0 } } },
    // 3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 32 }, max: 44, stride: 44, children: Span { first: 0, len: 0 } } },
    // 4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 5 NVKMS_QUERY_DPY_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 80 }, max: 92, stride: 92, children: Span { first: 0, len: 0 } } },
    // 6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2072, len: 35096 }, max: 37168, stride: 37168, children: Span { first: 0, len: 0 } } },
    // 7 NVKMS_VALIDATE_MODE_INDEX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 224, len: 512 }, max: 736, stride: 736, children: Span { first: 8, len: 1 } } },
    // 8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString
    Field { name: "request.pInfoString", off: 216, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 208, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 532, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 9 NVKMS_VALIDATE_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 304, len: 352 }, max: 656, stride: 656, children: Span { first: 10, len: 1 } } },
    // 10 NVKMS_VALIDATE_MODE.address.request.pInfoString
    Field { name: "request.pInfoString", off: 296, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 288, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 456, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 11 NVKMS_SET_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 155992, len: 15944 }, max: 171936, stride: 171936, children: Span { first: 12, len: 1 } } },
    // 12 NVKMS_SET_MODE.address.request.disp
    Field { name: "request.disp", off: 16, cond: None, kind: Kind::Array { count: 8, stride: 19496, limit: Limit::All, children: Span { first: 13, len: 1 } } },
    // 13 NVKMS_SET_MODE.address.request.disp.head
    Field { name: "head", off: 8, cond: None, kind: Kind::Array { count: 4, stride: 4872, limit: Limit::All, children: Span { first: 14, len: 2 } } },
    // 14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 360, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 376, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 16 NVKMS_SET_CURSOR_IMAGE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 48, len: 4 }, max: 52, stride: 52, children: Span { first: 0, len: 0 } } },
    // 17 NVKMS_MOVE_CURSOR.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 18 NVKMS_SET_LUT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 64, len: 4 }, max: 72, stride: 72, children: Span { first: 19, len: 2 } } },
    // 19 NVKMS_SET_LUT.address.request.common.input.pRamps
    Field { name: "request.common.input.pRamps", off: 32, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 20 NVKMS_SET_LUT.address.request.common.output.pRamps
    Field { name: "request.common.output.pRamps", off: 48, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 21 NVKMS_CHECK_LUT_NOTIFIER.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 1 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 22 NVKMS_IDLE_BASE_CHANNEL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 16 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 23 NVKMS_FLIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 3080 }, max: 3104, stride: 3104, children: Span { first: 24, len: 1 } } },
    // 24 NVKMS_FLIP.address.request.pFlipHead
    Field { name: "request.pFlipHead", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4488 }, copyback: CopyBack::None, max: 143616, stride: 4488, children: Span { first: 25, len: 2 } } },
    // 25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 88, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 104, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 28 NVKMS_REGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 144, len: 4 }, max: 152, stride: 152, children: Span { first: 29, len: 1 } } },
    // 29 NVKMS_REGISTER_SURFACE.address.request.planes
    Field { name: "request.planes", off: 16, cond: Some(Cond { off: 4, mask: 0xff, value: 0x0, ne: true }), kind: Kind::Array { count: 3, stride: 32, limit: Limit::Planes { off: 124 }, children: Span { first: 30, len: 1 } } },
    // 30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd
    Field { name: "u.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10080, none: -1 } },
    // 31 NVKMS_UNREGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 32 NVKMS_GRANT_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 33, len: 1 } } },
    // 33 NVKMS_GRANT_SURFACE.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 34 NVKMS_ACQUIRE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 35, len: 1 } } },
    // 35 NVKMS_ACQUIRE_SURFACE.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 36 NVKMS_RELEASE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 37 NVKMS_SET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 38 NVKMS_GET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 40 NVKMS_SET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 41 NVKMS_GET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 43 NVKMS_QUERY_FRAMELOCK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 16 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 8 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 24 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 47 NVKMS_GET_NEXT_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 40 }, max: 48, stride: 48, children: Span { first: 0, len: 0 } } },
    // 48 NVKMS_DECLARE_EVENT_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 49 NVKMS_CLEAR_UNICAST_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 50, len: 1 } } },
    // 50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd
    Field { name: "request.unicastEventFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 51 NVKMS_SET_LAYER_POSITION.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1192, len: 4 }, max: 1196, stride: 1196, children: Span { first: 0, len: 0 } } },
    // 52 NVKMS_GRAB_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 53 NVKMS_RELEASE_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 54 NVKMS_GRANT_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 55, len: 1 } } },
    // 55 NVKMS_GRANT_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 56 NVKMS_ACQUIRE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 24 }, max: 28, stride: 28, children: Span { first: 57, len: 1 } } },
    // 57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 58 NVKMS_REVOKE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 59 NVKMS_QUERY_DPY_CRC32.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 24 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 62 NVKMS_ALLOC_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 63 NVKMS_FREE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 64 NVKMS_JOIN_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2564, len: 4 }, max: 2568, stride: 2568, children: Span { first: 65, len: 1 } } },
    // 65 NVKMS_JOIN_SWAP_GROUP.address.request.member
    Field { name: "request.member", off: 4, cond: None, kind: Kind::Array { count: 128, stride: 20, limit: Limit::Count { off: 0, width: 4 }, children: Span { first: 66, len: 1 } } },
    // 66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd
    Field { name: "unicastEvent.fd", off: 12, cond: Some(Cond { off: 16, mask: 0xff, value: 0x0, ne: true }), kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 67 NVKMS_LEAVE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1028, len: 4 }, max: 1032, stride: 1032, children: Span { first: 0, len: 0 } } },
    // 68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 69, len: 1 } } },
    // 69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList
    Field { name: "request.pClipList", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 2, elem: 8 }, copyback: CopyBack::None, max: 524280, stride: 8, children: Span { first: 0, len: 0 } } },
    // 70 NVKMS_GRANT_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 71, len: 1 } } },
    // 71 NVKMS_GRANT_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 72 NVKMS_ACQUIRE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 73, len: 1 } } },
    // 73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 74 NVKMS_RELEASE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 75 NVKMS_SWITCH_MUX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 76 NVKMS_GET_MUX_STATE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 79 NVKMS_NOTIFY_VBLANK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 80, len: 1 } } },
    // 80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd
    Field { name: "request.unicastEvent.fd", off: 12, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 81 NVKMS_SET_FLIPLOCK_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 68, len: 4 }, max: 72, stride: 72, children: Span { first: 0, len: 0 } } },
    // 82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
];

static V595_71_05_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "NVKMS_ALLOC_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 0, len: 1 } },
    Ioctl { name: "NVKMS_FREE_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 1, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 1, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DISP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 2, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 2, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 3, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 3, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 4, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 4, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 5, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 5, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 6, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 6, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE_INDEX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 7, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 7, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 8, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 9, len: 1 } },
    Ioctl { name: "NVKMS_SET_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 9, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 11, len: 1 } },
    Ioctl { name: "NVKMS_SET_CURSOR_IMAGE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 10, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 16, len: 1 } },
    Ioctl { name: "NVKMS_MOVE_CURSOR", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 11, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 17, len: 1 } },
    Ioctl { name: "NVKMS_SET_LUT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 12, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 18, len: 1 } },
    Ioctl { name: "NVKMS_CHECK_LUT_NOTIFIER", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 13, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 21, len: 1 } },
    Ioctl { name: "NVKMS_IDLE_BASE_CHANNEL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 14, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 22, len: 1 } },
    Ioctl { name: "NVKMS_FLIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 15, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 23, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 16, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 27, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 17, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 28, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 18, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 31, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 19, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 32, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 20, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 34, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 21, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 36, len: 1 } },
    Ioctl { name: "NVKMS_SET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 22, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 37, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 23, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 38, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 24, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 39, len: 1 } },
    Ioctl { name: "NVKMS_SET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 25, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 40, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 26, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 41, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 27, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 42, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_FRAMELOCK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 28, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "NVKMS_SET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 29, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 44, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 30, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 45, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 31, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 46, len: 1 } },
    Ioctl { name: "NVKMS_GET_NEXT_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 32, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 47, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_EVENT_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 33, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "NVKMS_CLEAR_UNICAST_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 34, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 49, len: 1 } },
    Ioctl { name: "NVKMS_SET_LAYER_POSITION", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 37, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 51, len: 1 } },
    Ioctl { name: "NVKMS_GRAB_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 38, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 52, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 39, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 53, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 40, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 54, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 41, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 56, len: 1 } },
    Ioctl { name: "NVKMS_REVOKE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 42, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 58, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_CRC32", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 43, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 59, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 44, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 60, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 45, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 61, len: 1 } },
    Ioctl { name: "NVKMS_ALLOC_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 46, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 62, len: 1 } },
    Ioctl { name: "NVKMS_FREE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 47, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 63, len: 1 } },
    Ioctl { name: "NVKMS_JOIN_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 48, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 64, len: 1 } },
    Ioctl { name: "NVKMS_LEAVE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 49, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 67, len: 1 } },
    Ioctl { name: "NVKMS_SET_SWAP_GROUP_CLIP_LIST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 50, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 68, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 51, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 70, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 52, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 72, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 53, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 74, len: 1 } },
    Ioctl { name: "NVKMS_SWITCH_MUX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 54, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 75, len: 1 } },
    Ioctl { name: "NVKMS_GET_MUX_STATE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 55, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 76, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 56, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 77, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 57, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 78, len: 1 } },
    Ioctl { name: "NVKMS_NOTIFY_VBLANK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 58, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 79, len: 1 } },
    Ioctl { name: "NVKMS_SET_FLIPLOCK_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 59, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 81, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 60, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 82, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 61, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 83, len: 1 } },
    Ioctl { name: "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 62, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 84, len: 1 } },
];

static V595_71_05_LAYOUT: NvkmsLayout = NvkmsLayout {
    alloc_scrub: &[(40, 2), (44, 576)],
    alloc_reply_device: 628,
    alloc_reply_disps: 644,
    dpy_dynamic_scrub: &[(12, 2058)],
    set_cursor_image: NvkmsTarget { device: 0, disp: 4, what: 8 },
    move_cursor: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_lut: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_dpy_attribute: NvkmsTarget { device: 0, disp: 4, what: 8 },
    layer_position: NvkmsLayerPosition { device: 0, disps: 4, disp: Arr { off: 8, count: 8, stride: 148 }, heads: 0, head: Arr { off: 4, count: 4, stride: 36 } },
    flip: NvkmsFlipLayout { device: 0, ptr: 8, heads: 16, head_size: 4488, sd: 0, head: 4, layer: Arr { off: 200, count: 8, stride: 536 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    set_mode: NvkmsSetModeLayout { device: 0, commit: 4, disps: 8, disp: Arr { off: 16, count: 8, stride: 19496 }, heads: 0, head: Arr { off: 8, count: 4, stride: 4872 }, dpys: 0, layer: Arr { off: 472, count: 8, stride: 536 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    grant: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    acquire: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    revoke: NvkmsPerms { device: 0, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    event_interest: 0,
    events_allowed: 0x27,
    next_event_valid: 8,
    drm_grant_typed: true,
};

static V595_71_05: Table = Table {
    name: "v595_71_05",
    versions: Some((DriverVersion::new(595, 71, 5), DriverVersion::new(595, 99, 1))),
    ioctls: V595_71_05_IOCTLS,
    fields: V595_71_05_FIELDS,
    planes: &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1],
    nvkms: Some(&V595_71_05_LAYOUT),
};

static V595_99_02_FIELDS: &[Field] = &[
    // 0 NVKMS_ALLOC_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 624, len: 888 }, max: 1512, stride: 1512, children: Span { first: 0, len: 0 } } },
    // 1 NVKMS_FREE_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 2 NVKMS_QUERY_DISP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 164 }, max: 172, stride: 172, children: Span { first: 0, len: 0 } } },
    // 3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 32 }, max: 44, stride: 44, children: Span { first: 0, len: 0 } } },
    // 4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 5 NVKMS_QUERY_DPY_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 80 }, max: 92, stride: 92, children: Span { first: 0, len: 0 } } },
    // 6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2072, len: 35096 }, max: 37168, stride: 37168, children: Span { first: 0, len: 0 } } },
    // 7 NVKMS_VALIDATE_MODE_INDEX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 224, len: 512 }, max: 736, stride: 736, children: Span { first: 8, len: 1 } } },
    // 8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString
    Field { name: "request.pInfoString", off: 216, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 208, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 532, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 9 NVKMS_VALIDATE_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 304, len: 352 }, max: 656, stride: 656, children: Span { first: 10, len: 1 } } },
    // 10 NVKMS_VALIDATE_MODE.address.request.pInfoString
    Field { name: "request.pInfoString", off: 296, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 288, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 456, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 11 NVKMS_SET_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 155992, len: 15944 }, max: 171936, stride: 171936, children: Span { first: 12, len: 1 } } },
    // 12 NVKMS_SET_MODE.address.request.disp
    Field { name: "request.disp", off: 16, cond: None, kind: Kind::Array { count: 8, stride: 19496, limit: Limit::All, children: Span { first: 13, len: 1 } } },
    // 13 NVKMS_SET_MODE.address.request.disp.head
    Field { name: "head", off: 8, cond: None, kind: Kind::Array { count: 4, stride: 4872, limit: Limit::All, children: Span { first: 14, len: 2 } } },
    // 14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 360, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 376, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 16 NVKMS_SET_CURSOR_IMAGE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 48, len: 4 }, max: 52, stride: 52, children: Span { first: 0, len: 0 } } },
    // 17 NVKMS_MOVE_CURSOR.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 18 NVKMS_SET_LUT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 64, len: 4 }, max: 72, stride: 72, children: Span { first: 19, len: 2 } } },
    // 19 NVKMS_SET_LUT.address.request.common.input.pRamps
    Field { name: "request.common.input.pRamps", off: 32, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 20 NVKMS_SET_LUT.address.request.common.output.pRamps
    Field { name: "request.common.output.pRamps", off: 48, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 21 NVKMS_CHECK_LUT_NOTIFIER.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 1 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 22 NVKMS_IDLE_BASE_CHANNEL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 16 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 23 NVKMS_FLIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 3080 }, max: 3104, stride: 3104, children: Span { first: 24, len: 1 } } },
    // 24 NVKMS_FLIP.address.request.pFlipHead
    Field { name: "request.pFlipHead", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4488 }, copyback: CopyBack::None, max: 143616, stride: 4488, children: Span { first: 25, len: 2 } } },
    // 25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 88, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 104, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 28 NVKMS_REGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 144, len: 4 }, max: 152, stride: 152, children: Span { first: 29, len: 1 } } },
    // 29 NVKMS_REGISTER_SURFACE.address.request.planes
    Field { name: "request.planes", off: 16, cond: Some(Cond { off: 4, mask: 0xff, value: 0x0, ne: true }), kind: Kind::Array { count: 3, stride: 32, limit: Limit::Planes { off: 124 }, children: Span { first: 30, len: 1 } } },
    // 30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd
    Field { name: "u.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10080, none: -1 } },
    // 31 NVKMS_UNREGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 32 NVKMS_GRANT_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 33, len: 1 } } },
    // 33 NVKMS_GRANT_SURFACE.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 34 NVKMS_ACQUIRE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 35, len: 1 } } },
    // 35 NVKMS_ACQUIRE_SURFACE.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 36 NVKMS_RELEASE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 37 NVKMS_SET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 38 NVKMS_GET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 40 NVKMS_SET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 41 NVKMS_GET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 43 NVKMS_QUERY_FRAMELOCK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 16 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 8 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 24 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 47 NVKMS_GET_NEXT_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 40 }, max: 48, stride: 48, children: Span { first: 0, len: 0 } } },
    // 48 NVKMS_DECLARE_EVENT_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 49 NVKMS_CLEAR_UNICAST_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 50, len: 1 } } },
    // 50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd
    Field { name: "request.unicastEventFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 51 NVKMS_SET_LAYER_POSITION.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1192, len: 4 }, max: 1196, stride: 1196, children: Span { first: 0, len: 0 } } },
    // 52 NVKMS_GRAB_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 53 NVKMS_RELEASE_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 54 NVKMS_GRANT_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 55, len: 1 } } },
    // 55 NVKMS_GRANT_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 56 NVKMS_ACQUIRE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 24 }, max: 28, stride: 28, children: Span { first: 57, len: 1 } } },
    // 57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 58 NVKMS_REVOKE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 59 NVKMS_QUERY_DPY_CRC32.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 24 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 62 NVKMS_ALLOC_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 63 NVKMS_FREE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 64 NVKMS_JOIN_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2564, len: 4 }, max: 2568, stride: 2568, children: Span { first: 65, len: 1 } } },
    // 65 NVKMS_JOIN_SWAP_GROUP.address.request.member
    Field { name: "request.member", off: 4, cond: None, kind: Kind::Array { count: 128, stride: 20, limit: Limit::Count { off: 0, width: 4 }, children: Span { first: 66, len: 1 } } },
    // 66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd
    Field { name: "unicastEvent.fd", off: 12, cond: Some(Cond { off: 16, mask: 0xff, value: 0x0, ne: true }), kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 67 NVKMS_LEAVE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1028, len: 4 }, max: 1032, stride: 1032, children: Span { first: 0, len: 0 } } },
    // 68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 69, len: 1 } } },
    // 69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList
    Field { name: "request.pClipList", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 2, elem: 8 }, copyback: CopyBack::None, max: 524280, stride: 8, children: Span { first: 0, len: 0 } } },
    // 70 NVKMS_GRANT_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 71, len: 1 } } },
    // 71 NVKMS_GRANT_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 72 NVKMS_ACQUIRE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 73, len: 1 } } },
    // 73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 74 NVKMS_RELEASE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 75 NVKMS_SWITCH_MUX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 76 NVKMS_GET_MUX_STATE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 79 NVKMS_NOTIFY_VBLANK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 80, len: 1 } } },
    // 80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd
    Field { name: "request.unicastEvent.fd", off: 12, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 81 NVKMS_SET_FLIPLOCK_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 68, len: 4 }, max: 72, stride: 72, children: Span { first: 0, len: 0 } } },
    // 82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
];

static V595_99_02_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "NVKMS_ALLOC_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 0, len: 1 } },
    Ioctl { name: "NVKMS_FREE_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 1, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 1, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DISP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 2, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 2, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 3, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 3, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 4, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 4, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 5, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 5, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 6, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 6, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE_INDEX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 7, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 7, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 8, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 9, len: 1 } },
    Ioctl { name: "NVKMS_SET_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 9, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 11, len: 1 } },
    Ioctl { name: "NVKMS_SET_CURSOR_IMAGE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 10, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 16, len: 1 } },
    Ioctl { name: "NVKMS_MOVE_CURSOR", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 11, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 17, len: 1 } },
    Ioctl { name: "NVKMS_SET_LUT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 12, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 18, len: 1 } },
    Ioctl { name: "NVKMS_CHECK_LUT_NOTIFIER", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 13, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 21, len: 1 } },
    Ioctl { name: "NVKMS_IDLE_BASE_CHANNEL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 14, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 22, len: 1 } },
    Ioctl { name: "NVKMS_FLIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 15, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 23, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 16, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 27, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 17, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 28, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 18, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 31, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 19, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 32, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 20, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 34, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 21, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 36, len: 1 } },
    Ioctl { name: "NVKMS_SET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 22, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 37, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 23, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 38, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 24, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 39, len: 1 } },
    Ioctl { name: "NVKMS_SET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 25, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 40, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 26, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 41, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 27, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 42, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_FRAMELOCK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 28, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "NVKMS_SET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 29, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 44, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 30, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 45, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 31, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 46, len: 1 } },
    Ioctl { name: "NVKMS_GET_NEXT_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 32, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 47, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_EVENT_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 33, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "NVKMS_CLEAR_UNICAST_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 34, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 49, len: 1 } },
    Ioctl { name: "NVKMS_SET_LAYER_POSITION", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 37, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 51, len: 1 } },
    Ioctl { name: "NVKMS_GRAB_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 38, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 52, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 39, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 53, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 40, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 54, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 41, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 56, len: 1 } },
    Ioctl { name: "NVKMS_REVOKE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 42, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 58, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_CRC32", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 43, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 59, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 44, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 60, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 45, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 61, len: 1 } },
    Ioctl { name: "NVKMS_ALLOC_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 46, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 62, len: 1 } },
    Ioctl { name: "NVKMS_FREE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 47, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 63, len: 1 } },
    Ioctl { name: "NVKMS_JOIN_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 48, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 64, len: 1 } },
    Ioctl { name: "NVKMS_LEAVE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 49, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 67, len: 1 } },
    Ioctl { name: "NVKMS_SET_SWAP_GROUP_CLIP_LIST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 50, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 68, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 51, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 70, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 52, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 72, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 53, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 74, len: 1 } },
    Ioctl { name: "NVKMS_SWITCH_MUX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 54, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 75, len: 1 } },
    Ioctl { name: "NVKMS_GET_MUX_STATE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 55, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 76, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 56, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 77, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 57, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 78, len: 1 } },
    Ioctl { name: "NVKMS_NOTIFY_VBLANK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 58, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 79, len: 1 } },
    Ioctl { name: "NVKMS_SET_FLIPLOCK_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 59, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 81, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 60, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 82, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 61, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 83, len: 1 } },
    Ioctl { name: "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 62, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 84, len: 1 } },
];

static V595_99_02_LAYOUT: NvkmsLayout = NvkmsLayout {
    alloc_scrub: &[(40, 2), (44, 576)],
    alloc_reply_device: 628,
    alloc_reply_disps: 644,
    dpy_dynamic_scrub: &[(12, 2058)],
    set_cursor_image: NvkmsTarget { device: 0, disp: 4, what: 8 },
    move_cursor: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_lut: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_dpy_attribute: NvkmsTarget { device: 0, disp: 4, what: 8 },
    layer_position: NvkmsLayerPosition { device: 0, disps: 4, disp: Arr { off: 8, count: 8, stride: 148 }, heads: 0, head: Arr { off: 4, count: 4, stride: 36 } },
    flip: NvkmsFlipLayout { device: 0, ptr: 8, heads: 16, head_size: 4488, sd: 0, head: 4, layer: Arr { off: 200, count: 8, stride: 536 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    set_mode: NvkmsSetModeLayout { device: 0, commit: 4, disps: 8, disp: Arr { off: 16, count: 8, stride: 19496 }, heads: 0, head: Arr { off: 8, count: 4, stride: 4872 }, dpys: 0, layer: Arr { off: 472, count: 8, stride: 536 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    grant: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    acquire: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    revoke: NvkmsPerms { device: 0, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    event_interest: 0,
    events_allowed: 0x27,
    next_event_valid: 8,
    drm_grant_typed: true,
};

static V595_99_02: Table = Table {
    name: "v595_99_02",
    versions: Some((DriverVersion::new(595, 99, 2), DriverVersion::new(610, 57, 3))),
    ioctls: V595_99_02_IOCTLS,
    fields: V595_99_02_FIELDS,
    planes: &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1],
    nvkms: Some(&V595_99_02_LAYOUT),
};

static V610_57_04_FIELDS: &[Field] = &[
    // 0 NVKMS_ALLOC_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 624, len: 816 }, max: 1440, stride: 1440, children: Span { first: 0, len: 0 } } },
    // 1 NVKMS_FREE_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 2 NVKMS_QUERY_DISP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 164 }, max: 172, stride: 172, children: Span { first: 0, len: 0 } } },
    // 3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 32 }, max: 44, stride: 44, children: Span { first: 0, len: 0 } } },
    // 4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 5 NVKMS_QUERY_DPY_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 84 }, max: 96, stride: 96, children: Span { first: 0, len: 0 } } },
    // 6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2072, len: 35096 }, max: 37168, stride: 37168, children: Span { first: 0, len: 0 } } },
    // 7 NVKMS_VALIDATE_MODE_INDEX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 224, len: 512 }, max: 736, stride: 736, children: Span { first: 8, len: 1 } } },
    // 8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString
    Field { name: "request.pInfoString", off: 216, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 208, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 532, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 9 NVKMS_VALIDATE_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 304, len: 352 }, max: 656, stride: 656, children: Span { first: 10, len: 1 } } },
    // 10 NVKMS_VALIDATE_MODE.address.request.pInfoString
    Field { name: "request.pInfoString", off: 296, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 288, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 456, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 11 NVKMS_SET_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 170840, len: 15944 }, max: 186784, stride: 186784, children: Span { first: 12, len: 1 } } },
    // 12 NVKMS_SET_MODE.address.request.disp
    Field { name: "request.disp", off: 16, cond: None, kind: Kind::Array { count: 8, stride: 21352, limit: Limit::All, children: Span { first: 13, len: 1 } } },
    // 13 NVKMS_SET_MODE.address.request.disp.head
    Field { name: "head", off: 8, cond: None, kind: Kind::Array { count: 4, stride: 5336, limit: Limit::All, children: Span { first: 14, len: 2 } } },
    // 14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 360, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 376, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 16 NVKMS_SET_CURSOR_IMAGE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 48, len: 4 }, max: 52, stride: 52, children: Span { first: 0, len: 0 } } },
    // 17 NVKMS_MOVE_CURSOR.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 18 NVKMS_SET_LUT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 64, len: 4 }, max: 72, stride: 72, children: Span { first: 19, len: 2 } } },
    // 19 NVKMS_SET_LUT.address.request.common.input.pRamps
    Field { name: "request.common.input.pRamps", off: 32, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 20 NVKMS_SET_LUT.address.request.common.output.pRamps
    Field { name: "request.common.output.pRamps", off: 48, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 21 NVKMS_CHECK_LUT_NOTIFIER.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 1 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 22 NVKMS_IDLE_BASE_CHANNEL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 16 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 23 NVKMS_FLIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 3080 }, max: 3104, stride: 3104, children: Span { first: 24, len: 1 } } },
    // 24 NVKMS_FLIP.address.request.pFlipHead
    Field { name: "request.pFlipHead", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4952 }, copyback: CopyBack::None, max: 158464, stride: 4952, children: Span { first: 25, len: 2 } } },
    // 25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 88, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 104, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 28 NVKMS_REGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 144, len: 4 }, max: 152, stride: 152, children: Span { first: 29, len: 1 } } },
    // 29 NVKMS_REGISTER_SURFACE.address.request.planes
    Field { name: "request.planes", off: 16, cond: Some(Cond { off: 4, mask: 0xff, value: 0x0, ne: true }), kind: Kind::Array { count: 3, stride: 32, limit: Limit::Planes { off: 124 }, children: Span { first: 30, len: 1 } } },
    // 30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd
    Field { name: "u.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10080, none: -1 } },
    // 31 NVKMS_UNREGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 32 NVKMS_GRANT_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 33, len: 1 } } },
    // 33 NVKMS_GRANT_SURFACE.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 34 NVKMS_ACQUIRE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 35, len: 1 } } },
    // 35 NVKMS_ACQUIRE_SURFACE.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 36 NVKMS_RELEASE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 37 NVKMS_SET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 38 NVKMS_GET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 40 NVKMS_SET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 41 NVKMS_GET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 43 NVKMS_QUERY_FRAMELOCK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 16 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 8 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 24 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 47 NVKMS_GET_NEXT_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 40 }, max: 48, stride: 48, children: Span { first: 0, len: 0 } } },
    // 48 NVKMS_DECLARE_EVENT_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 49 NVKMS_CLEAR_UNICAST_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 50, len: 1 } } },
    // 50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd
    Field { name: "request.unicastEventFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 51 NVKMS_SET_LAYER_POSITION.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1192, len: 4 }, max: 1196, stride: 1196, children: Span { first: 0, len: 0 } } },
    // 52 NVKMS_GRAB_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 53 NVKMS_RELEASE_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 54 NVKMS_GRANT_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 55, len: 1 } } },
    // 55 NVKMS_GRANT_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 56 NVKMS_ACQUIRE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 24 }, max: 28, stride: 28, children: Span { first: 57, len: 1 } } },
    // 57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 58 NVKMS_REVOKE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 59 NVKMS_QUERY_DPY_CRC32.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 24 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 62 NVKMS_ALLOC_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 63 NVKMS_FREE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 64 NVKMS_JOIN_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2564, len: 4 }, max: 2568, stride: 2568, children: Span { first: 65, len: 1 } } },
    // 65 NVKMS_JOIN_SWAP_GROUP.address.request.member
    Field { name: "request.member", off: 4, cond: None, kind: Kind::Array { count: 128, stride: 20, limit: Limit::Count { off: 0, width: 4 }, children: Span { first: 66, len: 1 } } },
    // 66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd
    Field { name: "unicastEvent.fd", off: 12, cond: Some(Cond { off: 16, mask: 0xff, value: 0x0, ne: true }), kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 67 NVKMS_LEAVE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1028, len: 4 }, max: 1032, stride: 1032, children: Span { first: 0, len: 0 } } },
    // 68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 69, len: 1 } } },
    // 69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList
    Field { name: "request.pClipList", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 2, elem: 8 }, copyback: CopyBack::None, max: 524280, stride: 8, children: Span { first: 0, len: 0 } } },
    // 70 NVKMS_GRANT_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 71, len: 1 } } },
    // 71 NVKMS_GRANT_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 72 NVKMS_ACQUIRE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 73, len: 1 } } },
    // 73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 74 NVKMS_RELEASE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 75 NVKMS_SWITCH_MUX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 76 NVKMS_GET_MUX_STATE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 79 NVKMS_NOTIFY_VBLANK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 80, len: 1 } } },
    // 80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd
    Field { name: "request.unicastEvent.fd", off: 12, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 81 NVKMS_SET_FLIPLOCK_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 68, len: 4 }, max: 72, stride: 72, children: Span { first: 0, len: 0 } } },
    // 82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
];

static V610_57_04_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "NVKMS_ALLOC_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 0, len: 1 } },
    Ioctl { name: "NVKMS_FREE_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 1, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 1, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DISP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 2, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 2, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 3, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 3, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 4, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 4, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 5, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 5, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 6, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 6, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE_INDEX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 7, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 7, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 8, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 9, len: 1 } },
    Ioctl { name: "NVKMS_SET_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 9, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 11, len: 1 } },
    Ioctl { name: "NVKMS_SET_CURSOR_IMAGE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 10, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 16, len: 1 } },
    Ioctl { name: "NVKMS_MOVE_CURSOR", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 11, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 17, len: 1 } },
    Ioctl { name: "NVKMS_SET_LUT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 12, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 18, len: 1 } },
    Ioctl { name: "NVKMS_CHECK_LUT_NOTIFIER", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 13, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 21, len: 1 } },
    Ioctl { name: "NVKMS_IDLE_BASE_CHANNEL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 14, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 22, len: 1 } },
    Ioctl { name: "NVKMS_FLIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 15, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 23, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 16, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 27, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 17, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 28, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 18, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 31, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 19, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 32, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 20, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 34, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 21, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 36, len: 1 } },
    Ioctl { name: "NVKMS_SET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 22, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 37, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 23, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 38, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 24, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 39, len: 1 } },
    Ioctl { name: "NVKMS_SET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 25, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 40, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 26, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 41, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 27, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 42, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_FRAMELOCK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 28, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "NVKMS_SET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 29, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 44, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 30, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 45, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 31, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 46, len: 1 } },
    Ioctl { name: "NVKMS_GET_NEXT_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 32, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 47, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_EVENT_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 33, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "NVKMS_CLEAR_UNICAST_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 34, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 49, len: 1 } },
    Ioctl { name: "NVKMS_SET_LAYER_POSITION", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 37, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 51, len: 1 } },
    Ioctl { name: "NVKMS_GRAB_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 38, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 52, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 39, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 53, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 40, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 54, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 41, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 56, len: 1 } },
    Ioctl { name: "NVKMS_REVOKE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 42, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 58, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_CRC32", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 43, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 59, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 44, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 60, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 45, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 61, len: 1 } },
    Ioctl { name: "NVKMS_ALLOC_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 46, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 62, len: 1 } },
    Ioctl { name: "NVKMS_FREE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 47, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 63, len: 1 } },
    Ioctl { name: "NVKMS_JOIN_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 48, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 64, len: 1 } },
    Ioctl { name: "NVKMS_LEAVE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 49, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 67, len: 1 } },
    Ioctl { name: "NVKMS_SET_SWAP_GROUP_CLIP_LIST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 50, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 68, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 51, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 70, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 52, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 72, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 53, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 74, len: 1 } },
    Ioctl { name: "NVKMS_SWITCH_MUX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 54, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 75, len: 1 } },
    Ioctl { name: "NVKMS_GET_MUX_STATE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 55, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 76, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 56, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 77, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 57, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 78, len: 1 } },
    Ioctl { name: "NVKMS_NOTIFY_VBLANK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 58, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 79, len: 1 } },
    Ioctl { name: "NVKMS_SET_FLIPLOCK_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 59, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 81, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 60, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 82, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 61, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 83, len: 1 } },
    Ioctl { name: "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 62, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x300, arg_in_only: true, fields: Span { first: 84, len: 1 } },
];

static V610_57_04_LAYOUT: NvkmsLayout = NvkmsLayout {
    alloc_scrub: &[(40, 2), (44, 576)],
    alloc_reply_device: 628,
    alloc_reply_disps: 644,
    dpy_dynamic_scrub: &[(12, 2058)],
    set_cursor_image: NvkmsTarget { device: 0, disp: 4, what: 8 },
    move_cursor: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_lut: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_dpy_attribute: NvkmsTarget { device: 0, disp: 4, what: 8 },
    layer_position: NvkmsLayerPosition { device: 0, disps: 4, disp: Arr { off: 8, count: 8, stride: 148 }, heads: 0, head: Arr { off: 4, count: 4, stride: 36 } },
    flip: NvkmsFlipLayout { device: 0, ptr: 8, heads: 16, head_size: 4952, sd: 0, head: 4, layer: Arr { off: 216, count: 8, stride: 592 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    set_mode: NvkmsSetModeLayout { device: 0, commit: 4, disps: 8, disp: Arr { off: 16, count: 8, stride: 21352 }, heads: 0, head: Arr { off: 8, count: 4, stride: 5336 }, dpys: 0, layer: Arr { off: 488, count: 8, stride: 592 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    grant: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    acquire: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    revoke: NvkmsPerms { device: 0, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    event_interest: 0,
    events_allowed: 0x27,
    next_event_valid: 8,
    drm_grant_typed: true,
};

static V610_57_04: Table = Table {
    name: "v610_57_04",
    versions: Some((DriverVersion::new(610, 57, 4), DriverVersion::new(615, 71, 8))),
    ioctls: V610_57_04_IOCTLS,
    fields: V610_57_04_FIELDS,
    planes: &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1],
    nvkms: Some(&V610_57_04_LAYOUT),
};

static V615_71_09_FIELDS: &[Field] = &[
    // 0 NVKMS_ALLOC_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 624, len: 824 }, max: 1448, stride: 1448, children: Span { first: 0, len: 0 } } },
    // 1 NVKMS_FREE_DEVICE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 2 NVKMS_QUERY_DISP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 164 }, max: 172, stride: 172, children: Span { first: 0, len: 0 } } },
    // 3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 32 }, max: 44, stride: 44, children: Span { first: 0, len: 0 } } },
    // 4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 5 NVKMS_QUERY_DPY_STATIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 84 }, max: 96, stride: 96, children: Span { first: 0, len: 0 } } },
    // 6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2072, len: 35096 }, max: 37168, stride: 37168, children: Span { first: 0, len: 0 } } },
    // 7 NVKMS_VALIDATE_MODE_INDEX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 224, len: 512 }, max: 736, stride: 736, children: Span { first: 8, len: 1 } } },
    // 8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString
    Field { name: "request.pInfoString", off: 216, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 208, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 532, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 9 NVKMS_VALIDATE_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 304, len: 352 }, max: 656, stride: 656, children: Span { first: 10, len: 1 } } },
    // 10 NVKMS_VALIDATE_MODE.address.request.pInfoString
    Field { name: "request.pInfoString", off: 296, cond: None, kind: Kind::Ptr { dir: Dir::Out, len: Len::Count { off: 288, width: 4, elem: 1 }, copyback: CopyBack::Written { off: 456, width: 4 }, max: 2048, stride: 0, children: Span { first: 0, len: 0 } } },
    // 11 NVKMS_SET_MODE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 171864, len: 15944 }, max: 187808, stride: 187808, children: Span { first: 12, len: 1 } } },
    // 12 NVKMS_SET_MODE.address.request.disp
    Field { name: "request.disp", off: 16, cond: None, kind: Kind::Array { count: 8, stride: 21480, limit: Limit::All, children: Span { first: 13, len: 1 } } },
    // 13 NVKMS_SET_MODE.address.request.disp.head
    Field { name: "head", off: 8, cond: None, kind: Kind::Array { count: 4, stride: 5368, limit: Limit::All, children: Span { first: 14, len: 2 } } },
    // 14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 360, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 376, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 16 NVKMS_SET_CURSOR_IMAGE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 48, len: 4 }, max: 52, stride: 52, children: Span { first: 0, len: 0 } } },
    // 17 NVKMS_MOVE_CURSOR.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 18 NVKMS_SET_LUT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 64, len: 4 }, max: 72, stride: 72, children: Span { first: 19, len: 2 } } },
    // 19 NVKMS_SET_LUT.address.request.common.input.pRamps
    Field { name: "request.common.input.pRamps", off: 32, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 20 NVKMS_SET_LUT.address.request.common.output.pRamps
    Field { name: "request.common.output.pRamps", off: 48, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 21 NVKMS_IDLE_BASE_CHANNEL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 16 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 22 NVKMS_FLIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 3080 }, max: 3104, stride: 3104, children: Span { first: 23, len: 1 } } },
    // 23 NVKMS_FLIP.address.request.pFlipHead
    Field { name: "request.pFlipHead", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 16, width: 4, elem: 4984 }, copyback: CopyBack::None, max: 159488, stride: 4984, children: Span { first: 24, len: 2 } } },
    // 24 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps
    Field { name: "flip.lut.input.pRamps", off: 88, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps
    Field { name: "flip.lut.output.pRamps", off: 104, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Const(6144), copyback: CopyBack::None, max: 6144, stride: 6144, children: Span { first: 0, len: 0 } } },
    // 26 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 27 NVKMS_REGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 144, len: 4 }, max: 152, stride: 152, children: Span { first: 28, len: 1 } } },
    // 28 NVKMS_REGISTER_SURFACE.address.request.planes
    Field { name: "request.planes", off: 16, cond: Some(Cond { off: 4, mask: 0xff, value: 0x0, ne: true }), kind: Kind::Array { count: 3, stride: 32, limit: Limit::Planes { off: 124 }, children: Span { first: 29, len: 1 } } },
    // 29 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd
    Field { name: "u.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x10080, none: -1 } },
    // 30 NVKMS_UNREGISTER_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 31 NVKMS_GRANT_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 32, len: 1 } } },
    // 32 NVKMS_GRANT_SURFACE.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 33 NVKMS_ACQUIRE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 34, len: 1 } } },
    // 34 NVKMS_ACQUIRE_SURFACE.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 35 NVKMS_RELEASE_SURFACE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 36 NVKMS_SET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 37 NVKMS_GET_DPY_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 38 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 39 NVKMS_SET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 40 NVKMS_GET_DISP_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 8 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 41 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 24 }, max: 40, stride: 40, children: Span { first: 0, len: 0 } } },
    // 42 NVKMS_QUERY_FRAMELOCK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 16 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 43 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 44 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 8 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 45 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 24 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 46 NVKMS_GET_NEXT_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 40 }, max: 48, stride: 48, children: Span { first: 0, len: 0 } } },
    // 47 NVKMS_DECLARE_EVENT_INTEREST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 48 NVKMS_CLEAR_UNICAST_EVENT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 49, len: 1 } } },
    // 49 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd
    Field { name: "request.unicastEventFd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 50 NVKMS_SET_LAYER_POSITION.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1192, len: 4 }, max: 1196, stride: 1196, children: Span { first: 0, len: 0 } } },
    // 51 NVKMS_GRAB_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 52 NVKMS_RELEASE_OWNERSHIP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 4 }, max: 8, stride: 8, children: Span { first: 0, len: 0 } } },
    // 53 NVKMS_GRANT_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 54, len: 1 } } },
    // 54 NVKMS_GRANT_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 55 NVKMS_ACQUIRE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 24 }, max: 28, stride: 28, children: Span { first: 56, len: 1 } } },
    // 56 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 57 NVKMS_REVOKE_PERMISSIONS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 28, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 58 NVKMS_QUERY_DPY_CRC32.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 24 }, max: 36, stride: 36, children: Span { first: 0, len: 0 } } },
    // 59 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 60 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 61 NVKMS_ALLOC_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 62 NVKMS_FREE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 63 NVKMS_JOIN_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 2564, len: 4 }, max: 2568, stride: 2568, children: Span { first: 64, len: 1 } } },
    // 64 NVKMS_JOIN_SWAP_GROUP.address.request.member
    Field { name: "request.member", off: 4, cond: None, kind: Kind::Array { count: 128, stride: 20, limit: Limit::Count { off: 0, width: 4 }, children: Span { first: 65, len: 1 } } },
    // 65 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd
    Field { name: "unicastEvent.fd", off: 12, cond: Some(Cond { off: 16, mask: 0xff, value: 0x0, ne: true }), kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 66 NVKMS_LEAVE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 1028, len: 4 }, max: 1032, stride: 1032, children: Span { first: 0, len: 0 } } },
    // 67 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 68, len: 1 } } },
    // 68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList
    Field { name: "request.pClipList", off: 16, cond: None, kind: Kind::Ptr { dir: Dir::In, len: Len::Count { off: 8, width: 2, elem: 8 }, copyback: CopyBack::None, max: 524280, stride: 8, children: Span { first: 0, len: 0 } } },
    // 69 NVKMS_GRANT_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 70, len: 1 } } },
    // 70 NVKMS_GRANT_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 8, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 71 NVKMS_ACQUIRE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 4, len: 8 }, max: 12, stride: 12, children: Span { first: 72, len: 1 } } },
    // 72 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd
    Field { name: "request.fd", off: 0, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 73 NVKMS_RELEASE_SWAP_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 8, len: 4 }, max: 12, stride: 12, children: Span { first: 0, len: 0 } } },
    // 74 NVKMS_SWITCH_MUX.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 20, len: 4 }, max: 24, stride: 24, children: Span { first: 0, len: 0 } } },
    // 75 NVKMS_GET_MUX_STATE.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 76 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 8 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 77 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 0, len: 0 } } },
    // 78 NVKMS_NOTIFY_VBLANK.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 16, len: 4 }, max: 20, stride: 20, children: Span { first: 79, len: 1 } } },
    // 79 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd
    Field { name: "request.unicastEvent.fd", off: 12, cond: None, kind: Kind::FdIn { width: 4, kinds: 0x20000, none: -1 } },
    // 80 NVKMS_SET_FLIPLOCK_GROUP.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 68, len: 4 }, max: 72, stride: 72, children: Span { first: 0, len: 0 } } },
    // 81 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 24, len: 4 }, max: 32, stride: 32, children: Span { first: 0, len: 0 } } },
    // 82 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
    // 83 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address
    Field { name: "address", off: 8, cond: None, kind: Kind::Ptr { dir: Dir::InOut, len: Len::NvkmsParams, copyback: CopyBack::Range { off: 12, len: 4 }, max: 16, stride: 16, children: Span { first: 0, len: 0 } } },
];

static V615_71_09_IOCTLS: &[Ioctl] = &[
    Ioctl { name: "NVKMS_ALLOC_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 0, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 0, len: 1 } },
    Ioctl { name: "NVKMS_FREE_DEVICE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 1, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 1, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DISP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 2, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 2, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 3, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 3, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 4, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 4, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_STATIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 5, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 5, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_DYNAMIC_DATA", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 6, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 6, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE_INDEX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 7, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 7, len: 1 } },
    Ioctl { name: "NVKMS_VALIDATE_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 8, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 9, len: 1 } },
    Ioctl { name: "NVKMS_SET_MODE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 9, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 11, len: 1 } },
    Ioctl { name: "NVKMS_SET_CURSOR_IMAGE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 10, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 16, len: 1 } },
    Ioctl { name: "NVKMS_MOVE_CURSOR", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 11, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 17, len: 1 } },
    Ioctl { name: "NVKMS_SET_LUT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 12, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 18, len: 1 } },
    Ioctl { name: "NVKMS_IDLE_BASE_CHANNEL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 13, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 21, len: 1 } },
    Ioctl { name: "NVKMS_FLIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 14, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 22, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 15, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 26, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 16, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 27, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 17, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 30, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 18, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 31, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 19, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 33, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SURFACE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 20, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 35, len: 1 } },
    Ioctl { name: "NVKMS_SET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 21, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 36, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 22, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 37, len: 1 } },
    Ioctl { name: "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 23, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 38, len: 1 } },
    Ioctl { name: "NVKMS_SET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 24, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 39, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 25, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 40, len: 1 } },
    Ioctl { name: "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 26, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 41, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_FRAMELOCK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 27, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 42, len: 1 } },
    Ioctl { name: "NVKMS_SET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 28, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 43, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 29, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 44, len: 1 } },
    Ioctl { name: "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 30, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 45, len: 1 } },
    Ioctl { name: "NVKMS_GET_NEXT_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 31, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 46, len: 1 } },
    Ioctl { name: "NVKMS_DECLARE_EVENT_INTEREST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 32, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 47, len: 1 } },
    Ioctl { name: "NVKMS_CLEAR_UNICAST_EVENT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 33, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 48, len: 1 } },
    Ioctl { name: "NVKMS_SET_LAYER_POSITION", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 36, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 50, len: 1 } },
    Ioctl { name: "NVKMS_GRAB_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 37, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 51, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_OWNERSHIP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 38, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 52, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 39, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 53, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 40, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 55, len: 1 } },
    Ioctl { name: "NVKMS_REVOKE_PERMISSIONS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 41, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 57, len: 1 } },
    Ioctl { name: "NVKMS_QUERY_DPY_CRC32", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 42, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 58, len: 1 } },
    Ioctl { name: "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 43, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 59, len: 1 } },
    Ioctl { name: "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 44, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 60, len: 1 } },
    Ioctl { name: "NVKMS_ALLOC_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 45, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 61, len: 1 } },
    Ioctl { name: "NVKMS_FREE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 46, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 62, len: 1 } },
    Ioctl { name: "NVKMS_JOIN_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 47, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 63, len: 1 } },
    Ioctl { name: "NVKMS_LEAVE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 48, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 66, len: 1 } },
    Ioctl { name: "NVKMS_SET_SWAP_GROUP_CLIP_LIST", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 49, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 67, len: 1 } },
    Ioctl { name: "NVKMS_GRANT_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 50, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 69, len: 1 } },
    Ioctl { name: "NVKMS_ACQUIRE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 51, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 71, len: 1 } },
    Ioctl { name: "NVKMS_RELEASE_SWAP_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 52, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 73, len: 1 } },
    Ioctl { name: "NVKMS_SWITCH_MUX", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 53, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 74, len: 1 } },
    Ioctl { name: "NVKMS_GET_MUX_STATE", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 54, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 75, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 55, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 76, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 56, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 77, len: 1 } },
    Ioctl { name: "NVKMS_NOTIFY_VBLANK", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 57, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 78, len: 1 } },
    Ioctl { name: "NVKMS_SET_FLIPLOCK_GROUP", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 58, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 80, len: 1 } },
    Ioctl { name: "NVKMS_ENABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 59, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 81, len: 1 } },
    Ioctl { name: "NVKMS_DISABLE_VBLANK_SEM_CONTROL", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 60, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 82, len: 1 } },
    Ioctl { name: "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", class: Class::Modeset, cmd: 0xc0106d00, nvkms_cmd: 61, size: 16, exec: Exec::Executor, special: Special::NvkmsParams, policy: 0x100, arg_in_only: true, fields: Span { first: 83, len: 1 } },
];

static V615_71_09_LAYOUT: NvkmsLayout = NvkmsLayout {
    alloc_scrub: &[(40, 2), (44, 576)],
    alloc_reply_device: 628,
    alloc_reply_disps: 644,
    dpy_dynamic_scrub: &[(12, 2058)],
    set_cursor_image: NvkmsTarget { device: 0, disp: 4, what: 8 },
    move_cursor: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_lut: NvkmsTarget { device: 0, disp: 4, what: 8 },
    set_dpy_attribute: NvkmsTarget { device: 0, disp: 4, what: 8 },
    layer_position: NvkmsLayerPosition { device: 0, disps: 4, disp: Arr { off: 8, count: 8, stride: 148 }, heads: 0, head: Arr { off: 4, count: 4, stride: 36 } },
    flip: NvkmsFlipLayout { device: 0, ptr: 8, heads: 16, head_size: 4984, sd: 0, head: 4, layer: Arr { off: 248, count: 8, stride: 592 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    set_mode: NvkmsSetModeLayout { device: 0, commit: 4, disps: 8, disp: Arr { off: 16, count: 8, stride: 21480 }, heads: 0, head: Arr { off: 8, count: 4, stride: 5368 }, dpys: 0, layer: Arr { off: 520, count: 8, stride: 592 }, use_syncpt: 60, sync_specified: 96, awaken: 52 },
    grant: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    acquire: NvkmsPerms { device: 4, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    revoke: NvkmsPerms { device: 0, ptype: 8, flip: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 1 } }, modeset: NvkmsPermArr { disp: None, head: Arr { off: 12, count: 4, stride: 4 } } },
    event_interest: 0,
    events_allowed: 0x27,
    next_event_valid: 8,
    drm_grant_typed: true,
};

static V615_71_09: Table = Table {
    name: "v615_71_09",
    versions: Some((DriverVersion::new(615, 71, 9), DriverVersion::new(999, 999, 999))),
    ioctls: V615_71_09_IOCTLS,
    fields: V615_71_09_FIELDS,
    planes: &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1],
    nvkms: Some(&V615_71_09_LAYOUT),
};

/// DRM core and nvidia-drm entries (classes Render and Kms), for any host version.
pub static DRM_TABLE: &Table = &DRM;

/// NVKMS entries, one table per range of host driver versions.
pub static MODESET_TABLES: &[&Table] = &[&V535_129_03, &V580_178_04, &V595_71_05, &V595_99_02, &V610_57_04, &V615_71_09];

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
