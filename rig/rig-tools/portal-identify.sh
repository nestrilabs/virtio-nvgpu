#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Run portal-identify.py (is a real screen share's buffer NVKMS memory?) with
# python, dbus-python, PyGObject, GStreamer and PipeWire's GStreamer plugin
# from nixpkgs. Run it on the desktop as the desktop user: the portal's
# picker appears there. Arguments go to the script (--inject SOCKET,
# --render PATH); --check only checks that the environment imports.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
expr='let pkgs = (builtins.getFlake "nixpkgs").legacyPackages.${builtins.currentSystem};
  in pkgs.buildEnv {
    name = "nvgpu-portal-identify";
    paths = [
      (pkgs.python3.withPackages (p: [ p.dbus-python p.pygobject3 ]))
      pkgs.gst_all_1.gstreamer.out pkgs.gst_all_1.gst-plugins-base pkgs.pipewire.out
      pkgs.gobject-introspection pkgs.glib.out
    ];
    pathsToLink = [ "/bin" "/lib" ];
  }'
env=$(nix build --no-link --print-out-paths --impure --expr "$expr")
# A chroot store's paths are visible only inside nix's own namespace.
exec nix shell "$env" -c env \
    GI_TYPELIB_PATH="$env/lib/girepository-1.0" \
    GST_PLUGIN_SYSTEM_PATH_1_0="$env/lib/gstreamer-1.0" \
    "$env/bin/python3" "$here/portal-identify.py" "$@"
