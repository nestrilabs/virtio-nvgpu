#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""portal-identify -- is a real screen share's buffer NVKMS memory?

What the capture helper will get from the desktop's portal, checked the way
the backend checks it: a ScreenCast session through
org.freedesktop.portal.ScreenCast (the host's picker appears: choose a
monitor or a window), its PipeWire stream consumed as DMA-BUF with
GStreamer's pipewiresrc, and the first frame's dma-buf imported into this
GPU's render node and asked GEM_IDENTIFY_OBJECT -- NVKMS is what
--inject-socket accepts; anything else it refuses. With --inject SOCKET it
also offers the buffer to a running backend's inject socket (rig/run-guest.sh
--inject, or a backend started by hand) and prints the answer.

Run it on the desktop, as the desktop user, outside any sandbox that hides
the session bus or PipeWire, through its wrapper (which brings python,
dbus-python, PyGObject, GStreamer and PipeWire's plugin from nixpkgs):

  rig/rig-tools/portal-identify.sh [--inject SOCKET]
  rig/rig-tools/portal-identify.sh --selftest   # the environment only

It needs xdg-desktop-portal with the Hyprland backend running, and a click
in its picker. The project's own tests never run it: they may not open a
picker on the desktop.
"""
import argparse
import fcntl
import os
import socket
import struct
import sys

import dbus
from dbus.mainloop.glib import DBusGMainLoop
import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstAllocators", "1.0")
from gi.repository import GLib, Gst, GstAllocators  # noqa: E402

DRM_IOCTL_PRIME_FD_TO_HANDLE = 0xC00C642E
DRM_IOCTL_GEM_CLOSE = 0x40086409
DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT = 0xC008644E
TYPES = {0: "NVKMS", 1: "DMABUF (another device's memory)", 2: "USERMEMORY"}


def identify(render, dmabuf_fd):
    """The GEM_IDENTIFY_OBJECT answer for the dma-buf in `render`."""
    arg = bytearray(struct.pack("<IIi", 0, 0, dmabuf_fd))
    fcntl.ioctl(render, DRM_IOCTL_PRIME_FD_TO_HANDLE, arg)
    handle = struct.unpack_from("<I", arg)[0]
    ident = bytearray(struct.pack("<II", handle, 0))
    try:
        fcntl.ioctl(render, DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT, ident)
        return struct.unpack_from("<I", ident, 4)[0]
    finally:
        fcntl.ioctl(render, DRM_IOCTL_GEM_CLOSE, bytearray(struct.pack("<II", handle, 0)))


def inject(path, fd, w, h, fourcc, modifier, offset, stride):
    """HELLO and IMPORT on a backend's inject socket; the IMPORT's status."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    s.connect(path)
    s.send(struct.pack("<IIII", 1, 1, 0, 0))
    hello = s.recv(48)
    if struct.unpack_from("<i", hello, 4)[0] != 0:
        return "HELLO refused"
    req = struct.pack("<IIIIIIQ4I4I", 2, 1, w, h, fourcc, 0, modifier, offset, 0, 0, 0, stride, 0, 0, 0)
    s.sendmsg([req], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("i", fd))])
    r = s.recv(48)
    status, ident = struct.unpack_from("<iI", r, 4)
    return "accepted as id %d" % ident if status == 0 else "refused, errno %d" % -status


class Portal:
    """The ScreenCast handshake, one Request at a time."""

    def __init__(self):
        DBusGMainLoop(set_as_default=True)
        self.bus = dbus.SessionBus()
        obj = self.bus.get_object("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop")
        self.sc = dbus.Interface(obj, "org.freedesktop.portal.ScreenCast")
        self.sender = self.bus.get_unique_name()[1:].replace(".", "_")
        self.n = 0
        self.loop = GLib.MainLoop()

    def call(self, method, *args):
        self.n += 1
        token = "nvgpu%d" % self.n
        path = "/org/freedesktop/portal/desktop/request/%s/%s" % (self.sender, token)
        result = {}

        def done(response, results):
            result["r"] = (response, results)
            self.loop.quit()

        self.bus.add_signal_receiver(done, "Response", "org.freedesktop.portal.Request", path=path)
        opts = dict(args[-1])
        opts["handle_token"] = token
        getattr(self.sc, method)(*args[:-1], opts)
        self.loop.run()
        response, results = result["r"]
        if response != 0:
            sys.exit("portal: %s answered %d (cancelled?)" % (method, response))
        return results


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--render", default="/dev/dri/renderD128")
    ap.add_argument("--inject", metavar="SOCKET")
    ap.add_argument("--selftest", action="store_true", help="check the environment, ask no portal")
    a = ap.parse_args()
    if a.selftest:
        Gst.init(None)
        gi.require_version("GstVideo", "1.0")
        from gi.repository import GstVideo  # noqa: F401

        ok = Gst.ElementFactory.find("pipewiresrc") is not None
        caps = Gst.Caps.from_string("video/x-raw(memory:DMABuf),format=DMA_DRM")
        print("selftest: pipewiresrc %s, DMA_DRM caps %s, GStreamer %s"
              % ("found" if ok else "MISSING", "parse" if caps else "do not parse", Gst.version_string()))
        sys.exit(0 if ok and caps else 1)

    p = Portal()
    s = p.call("CreateSession", {"session_handle_token": "nvgpu"})
    session = s["session_handle"]
    p.call("SelectSources", session, {"types": dbus.UInt32(1 | 2), "multiple": False})
    streams = p.call("Start", session, "", {})["streams"]
    node = int(streams[0][0])
    fd = p.sc.OpenPipeWireRemote(session, {}).take()
    print("portal: stream node %d, PipeWire remote fd %d" % (node, fd))

    Gst.init(None)
    pipe = Gst.parse_launch(
        "pipewiresrc fd=%d path=%d do-timestamp=true ! "
        "video/x-raw(memory:DMABuf),format=DMA_DRM ! appsink name=sink max-buffers=1 drop=true" % (fd, node)
    )
    sink = pipe.get_by_name("sink")
    pipe.set_state(Gst.State.PLAYING)
    sample = sink.emit("try-pull-sample", 10 * Gst.SECOND)
    if sample is None:
        sys.exit("no DMA-BUF frame in 10 s (the portal may have fallen back to shared memory)")
    caps = sample.get_caps().get_structure(0)
    drm = caps.get_string("drm-format") or "?"
    w, h = caps.get_int("width")[1], caps.get_int("height")[1]
    mem = sample.get_buffer().peek_memory(0)
    if not GstAllocators.is_dmabuf_memory(mem):
        sys.exit("the frame is not a dma-buf (%s)" % mem)
    dfd = GstAllocators.dmabuf_memory_get_fd(mem)
    meta = GstVideo_meta(sample.get_buffer())
    render = os.open(a.render, os.O_RDWR | os.O_CLOEXEC)
    t = identify(render, dfd)
    print("frame: %dx%d %s, dma-buf fd %d: GEM_IDENTIFY_OBJECT on %s says %s"
          % (w, h, drm, dfd, a.render, TYPES.get(t, hex(t))))
    print("=> %s" % ("NVKMS memory: --inject-socket takes it" if t == 0 else "refused by --inject-socket"))
    if a.inject:
        fourcc_s, _, mod_s = drm.partition(":")
        fourcc = struct.unpack("<I", fourcc_s.encode()[:4].ljust(4))[0]
        modifier = int(mod_s, 16) if mod_s else 0
        offset, stride = meta if meta else (0, w * 4)
        print("inject: %s" % inject(a.inject, dfd, w, h, fourcc, modifier, offset, stride))
    pipe.set_state(Gst.State.NULL)


def GstVideo_meta(buf):
    """The first plane's offset and stride, from GstVideoMeta, if there is one."""
    try:
        gi.require_version("GstVideo", "1.0")
        from gi.repository import GstVideo

        m = GstVideo.buffer_get_video_meta(buf)
        return (m.offset[0], m.stride[0]) if m else None
    except Exception:  # noqa: BLE001
        return None


if __name__ == "__main__":
    main()
