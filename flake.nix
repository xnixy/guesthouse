{
  description = "Guesthouse: Windows and Linux machines on KVM, with the real GPU driver";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      packages.${system} = rec {
        guesthouse = pkgs.callPackage ./packaging/nix/package.nix { };
        default = guesthouse;
      };

      overlays.default = final: _: {
        guesthouse = final.callPackage ./packaging/nix/package.nix { };
      };

      nixosModules.default = { lib, pkgs, ... }: {
        # A key, for module deduplication
        key = "guesthouse";
        imports = [ ./packaging/nix/module.nix ];
        config.programs.guesthouse.package =
          lib.mkOptionDefault
            self.packages.${pkgs.stdenv.hostPlatform.system}.guesthouse;
      };

      checks.${system}.module = pkgs.testers.runNixOSTest (import ./packaging/nix/test.nix self);

      # Not a check, as it needs nested KVM.
      legacyPackages.${system} = {
        machine-test = pkgs.testers.runNixOSTest (import ./packaging/nix/machine-test.nix self);
        jail-test = pkgs.testers.runNixOSTest (import ./packaging/nix/jail-test.nix self);
      };
    };
}
