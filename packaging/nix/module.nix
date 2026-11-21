# What install.sh sets up on other distributions: the program, KVM, the bridge helper, OpenGL and the
# UEFI firmware.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.guesthouse;
  firmware = pkgs.OVMFFull.fd;
  nvidia = config.hardware.nvidia.package;
  # The NVIDIA driver branches Guesthouse is tested on, from TESTED_VERSIONS in
  # crates/app-core/src/linux/driver_versions.rs.
  testedBranches = [
    "615"
    "620"
  ];
  untestedNvidia =
    config.hardware.nvidia.enabled && !lib.elem (lib.versions.major nvidia.version) testedBranches;

  # QEMU's firmware.json format. The app wants the code with Secure Boot and SMM, in a copy with
  # Microsoft's keys already in the variables for Windows 11 and in one without.
  descriptor =
    { description, variables, enrolled }:
    builtins.toJSON {
      inherit description;
      interface-types = [ "uefi" ];
      mapping = {
        device = "flash";
        mode = "split";
        executable = {
          filename = "${firmware}/FV/OVMF_CODE.fd";
          format = "raw";
        };
        nvram-template = {
          filename = "${firmware}/FV/${variables}";
          format = "raw";
        };
      };
      targets = [
        {
          architecture = "x86_64";
          machines = [ "pc-q35-*" ];
        }
      ];
      features = [
        "acpi-s3"
      ]
      ++ lib.optional enrolled "enrolled-keys"
      ++ [
        "requires-smm"
        "secure-boot"
        "verbose-dynamic"
      ];
      tags = [ ];
    };
in
{
  options.programs.guesthouse = {
    enable = lib.mkEnableOption "Guesthouse, which runs Windows and Linux machines on KVM with the real GPU driver";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The Guesthouse package to use.";
    };

    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "alice" ];
      description = ''
        Users who may start machines without being logged in at the desktop, as over SSH. They join the
        kvm group, for /dev/udmabuf, which a machine on an AMD card needs. Whoever is logged in at the
        desktop gets that device without this, and /dev/kvm is open to everyone on NixOS.
      '';
    };

    bridges = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "br0" ];
      description = ''
        Bridges that machines may attach to, which the bridge helper only allows for those named here.
        With none, /etc/qemu/bridge.conf isn't written.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    warnings = lib.optional untestedNvidia (
      lib.concatStringsSep " " [
        "Guesthouse is tested with NVIDIA's ${lib.concatStringsSep " and " testedBranches} drivers,"
        "and this system has ${nvidia.version}. Plain machines work, but a Linux machine gets no GPU,"
        "and a Windows machine's GPU hasn't been tried with it."
        "docs/nixos.md in Guesthouse's repo says how to pin a tested one."
      ]
    );

    environment.systemPackages = [
      cfg.package
      # The app reads card names from its pci.ids, and the AMD backend from libdrm's list, which names
      # each card by revision.
      pkgs.hwdata
      pkgs.libdrm.out
    ];
    environment.pathsToLink = [
      "/share/hwdata"
      "/share/libdrm"
    ];

    users.users = lib.genAttrs cfg.users (_: {
      extraGroups = [ "kvm" ];
    });

    hardware.graphics.enable = lib.mkDefault true;

    # A machine with a bridged network starts QEMU's helper, which needs root to attach to the bridge.
    security.wrappers.guesthouse-bridge-helper = {
      setuid = true;
      owner = "root";
      group = "root";
      source = "${cfg.package}/lib/guesthouse/qemu/build/qemu-bridge-helper";
    };

    environment.etc = {
      "qemu/bridge.conf" = lib.mkIf (cfg.bridges != [ ]) {
        text = lib.concatMapStrings (name: "allow ${name}\n") cfg.bridges;
      };
      "qemu/firmware/40-guesthouse-ovmf-secure-boot-enrolled.json".text = descriptor {
        description = "UEFI firmware with Secure Boot and Microsoft's keys enrolled";
        variables = "OVMF_VARS.ms.fd";
        enrolled = true;
      };
      "qemu/firmware/50-guesthouse-ovmf-secure-boot.json".text = descriptor {
        description = "UEFI firmware with Secure Boot, and no keys enrolled";
        variables = "OVMF_VARS.fd";
        enrolled = false;
      };
    };
  };
}
