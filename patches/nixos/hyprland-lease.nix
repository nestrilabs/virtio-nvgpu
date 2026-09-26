# SPDX-License-Identifier: Apache-2.0
# NixOS module: run Hyprland with virtio-nvgpu's DRM-lease patches (a `leasable` monitor rule), linked against a
# patched aquamarine.
#
# Keep these files together in one directory; the defaults below find the patches next to this file:
#   hyprland-lease.nix
#   0001-lease-desktop-outputs-efb5099.patch   Hyprland 0.56.2 (efb50993, tag v0.56.2)
#   0001-keep-leased-crtcs-1a10fe2.patch       aquamarine 1a10fe26 (0.14.0+6; what Hyprland's flake pins for 0.56.2)
#   0001-keep-leased-crtcs-0.15.1.patch        aquamarine 0.15.1 (what nixpkgs builds Hyprland 0.56.2 with)
#
# Use:
#   imports = [ ./hyprland-lease/hyprland-lease.nix ];
#   programs.hyprland.enable = true;
#   programs.hyprland.lease.enable = true;
#   # the Hyprland to patch; default pkgs.hyprland. With the Hyprland flake:
#   programs.hyprland.lease.basePackage = inputs.hyprland.packages.${pkgs.stdenv.hostPlatform.system}.hyprland;
#
# The module sets programs.hyprland.package to the patched build, above normal priority, so an existing
# `programs.hyprland.package = ...;` line (or the Hyprland flake module's mkDefault) no longer decides the package:
# move its value to lease.basePackage. The patched package is also available as
# config.programs.hyprland.lease.finalPackage, e.g. for home-manager's wayland.windowManager.hyprland.package.
#
# How the patching works, whatever the base: basePackage.override receives the arguments Hyprland was built with, so
# the aquamarine it links is replaced by that same aquamarine plus our patch (the Hyprland flake takes aquamarine from
# its own flake input, not from pkgs, and this still catches it). The Hyprland patch is appended to the package's own
# patches, so whatever the base already does (a postPatch, other overrideAttrs) stays.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    mkOverride
    types
    hasPrefix
    ;

  cfg = config.programs.hyprland.lease;

  # The Hyprland patch also carries unit and hyprtester tests. The Hyprland flake leaves tests/ and hyprtester/ out of its
  # source unless it builds them, so they are filtered out of the patch that goes into the package.
  hyprlandSrcPatch =
    pkgs.runCommand "hyprland-lease-src.patch" { nativeBuildInputs = [ pkgs.buildPackages.patchutils ]; }
      ''
        filterdiff -p1 -x 'tests/*' -x 'hyprtester/*' ${cfg.hyprlandPatch} > $out
        grep -q '^+++ b/src/protocols/DRMLease.cpp' $out
      '';

  aquamarinePatchFor =
    aquamarine:
    if cfg.aquamarinePatch != null then
      cfg.aquamarinePatch
    else if hasPrefix "0.15." aquamarine.version then
      ./0001-keep-leased-crtcs-0.15.1.patch
    else
      ./0001-keep-leased-crtcs-1a10fe2.patch;

  patchAquamarine =
    aquamarine:
    aquamarine.overrideAttrs (old: {
      patches = (old.patches or [ ]) ++ [ (aquamarinePatchFor aquamarine) ];
    });

  finalPackage =
    (cfg.basePackage.override (old: {
      aquamarine = patchAquamarine old.aquamarine;
    })).overrideAttrs
      (old: {
        patches = (old.patches or [ ]) ++ [ hyprlandSrcPatch ];
      });
in
{
  options.programs.hyprland.lease = {
    enable = mkEnableOption "Hyprland with virtio-nvgpu's DRM-lease patches (the `leasable` monitor rule)";

    basePackage = mkOption {
      type = types.package;
      default = pkgs.hyprland;
      defaultText = lib.literalExpression "pkgs.hyprland";
      example = lib.literalExpression "inputs.hyprland.packages.\${pkgs.stdenv.hostPlatform.system}.hyprland";
      description = ''
        The unpatched Hyprland to patch: nixpkgs' `hyprland` or the Hyprland flake's `hyprland` package. It must be
        Hyprland 0.56.2 (commit efb50993), which is what the patch is against.
      '';
    };

    hyprlandPatch = mkOption {
      type = types.path;
      default = ./0001-lease-desktop-outputs-efb5099.patch;
      description = "The Hyprland patch.";
    };

    aquamarinePatch = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        The aquamarine patch. null picks the one for the aquamarine basePackage is built with: the 0.15.1 patch
        for aquamarine 0.15.x (nixpkgs), the 1a10fe26 patch otherwise (the Hyprland flake's 0.14.0+date=… pin).
      '';
    };

    checkVersion = mkOption {
      type = types.bool;
      default = true;
      description = "Refuse to evaluate when basePackage is not Hyprland 0.56.2, instead of failing later in the patch phase.";
    };

    finalPackage = mkOption {
      type = types.package;
      readOnly = true;
      default = finalPackage;
      defaultText = lib.literalExpression "the patched basePackage";
      description = "The patched Hyprland, linked against the patched aquamarine.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = !cfg.checkVersion || hasPrefix "0.56.2" cfg.basePackage.version;
        message = ''
          programs.hyprland.lease: basePackage is Hyprland ${cfg.basePackage.version}, but the lease patch is against
          0.56.2 (efb50993). Pin Hyprland to 0.56.2, or rebase the patch and set checkVersion = false.
        '';
      }
    ];

    # 90: above a plain definition (100) and the Hyprland flake module's mkDefault, below mkForce
    programs.hyprland.package = mkOverride 90 cfg.finalPackage;
  };
}
