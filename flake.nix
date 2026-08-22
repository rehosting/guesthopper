{
  description = "guesthopper: guest command server (+ host client) for penguin, cross-built for every guest arch";

  nixConfig = {
    extra-substituters = [ "https://rehosting-tools.cachix.org" ];
    extra-trusted-public-keys = [
      "rehosting-tools.cachix.org-1:iNKSaFwG7MfGn6Fk7oTmIcLHqfffQ+cQIE5gWc6MlY0="
    ];
  };

  # Pinned to the same nixpkgs commit as penguin / penguin-tools so the cross
  # toolchains and rust closures are byte-identical and shared through Cachix.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/b6067cc0127d4db9c26c79e4de0513e58d0c40c9";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      archMatrix = import ./src/archs.nix;

      pkgs = import nixpkgs {
        inherit system;
        config.allowUnsupportedSystem = true;
      };
      lib = pkgs.lib;

      mkMuslCrossPkgs = archKey:
        import nixpkgs {
          inherit system;
          config.allowUnsupportedSystem = true;
          crossSystem = archMatrix.${archKey}.muslCrossSystem;
        };

      # rust has no usable mips64 musl target (n64 muslabi64 is tier-3, no std),
      # so -- like the upstream Docker builds -- the mips64 guests reuse the
      # 32-bit mips Rust binary (o32 binaries run on 64-bit MIPS guests).
      rustBuildArch = archKey:
        {
          mips64eb = "mipseb";
          mips64el = "mipsel";
        }.${archKey} or archKey;

      mkGuesthopper = archKey:
        import ./src/mk-guest-rust-tool.nix {
          crossPkgs = mkMuslCrossPkgs (rustBuildArch archKey);
          src = self;
          pname = "guesthopper";
          version = "0.0.1";
        };
      guesthopperBins = lib.mapAttrs (archKey: _: mkGuesthopper archKey) archMatrix;

      # Per-arch packages exposed individually for partial builds / debugging.
      perArchPackages = builtins.listToAttrs (
        lib.mapAttrsToList
          (archKey: _: {
            name = "guesthopper-${archMatrix.${archKey}.penguinName}";
            value = guesthopperBins.${archKey};
          })
          archMatrix
      );

      # dist: exactly the fragment of /igloo_static this tool owns, so penguin
      # can `cp -a ${guesthopper}/. igloo_static/`. Mirrors
      # penguin-tools/src/mk-dist-root.nix guesthopper staging:
      #   guesthopper/guesthopper.<penguinName>            per-arch binary
      #   guesthopper/guesthopper.<compat> -> canonical    legacy arch-name aliases
      #   guesthopper/guest_cmd.py                         host-side client (arch-independent)
      #   guesthopper/telnet_gateway.py                    host-side telnet front door (arch-independent)
      dist = pkgs.runCommand "guesthopper-dist"
        {
          nativeBuildInputs = with pkgs.buildPackages; [ coreutils ];
        }
        ''
          set -euo pipefail
          mkdir -p "$out/guesthopper"
          ${lib.concatStringsSep "\n" (
            lib.mapAttrsToList
              (archKey: _:
                let
                  spec = archMatrix.${archKey};
                  pn = spec.penguinName;
                  compatNames = spec.compatNames or [ ];
                  guesthopper = guesthopperBins.${archKey};
                in
                ''
                  cp ${guesthopper}/bin/guesthopper "$out/guesthopper/guesthopper.${pn}"
                  ${lib.concatMapStringsSep "\n"
                    (compat: ''ln -sfn "guesthopper.${pn}" "$out/guesthopper/guesthopper.${compat}"'')
                    compatNames}
                '')
              archMatrix
          )}

          # Host-side client (arch-independent), read by penguin at
          # /igloo_static/guesthopper/guest_cmd.py.
          cp ${self}/guest_cmd.py "$out/guesthopper/guest_cmd.py"
          # Host-side telnet front door (imports guest_cmd, so it must sit
          # beside it). Penguin runs it at /igloo_static/guesthopper/telnet_gateway.py.
          cp ${self}/telnet_gateway.py "$out/guesthopper/telnet_gateway.py"

          chmod -R u+w "$out"
        '';
    in
    {
      packages.${system} = perArchPackages // {
        inherit dist;
        default = dist;
      };

      checks.${system} = {
        inherit dist;
      };
    };
}
