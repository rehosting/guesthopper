# Build a Rust guest utility cross-compiled for one target arch as a
# static-musl binary. Copied from penguin-tools/src/mk-guest-rust-tool.nix so
# this repo builds itself standalone.
#
# Unlike the upstream Docker build, we do NOT use the crate's .cargo/config:
# it hardcodes linker paths into a /opt/cross toolchain that only exists inside
# the embedded-toolchains image. nixpkgs' cross rustPlatform injects the correct
# linker via CARGO_TARGET_<triple>_LINKER, so we strip that file.
#
#   crossPkgs -- a musl cross nixpkgs (archs.nix muslCrossSystem) for a fully
#                static guest binary.
#   src       -- the (forked) crate source tree (must contain Cargo.lock).
#   pname     -- derivation name.
#   binName   -- the produced binary (defaults to pname).
{ crossPkgs
, src
, pname
, version ? "0"
, binName ? pname
}:

crossPkgs.rustPlatform.buildRustPackage {
  inherit pname version src;

  cargoLock.lockFile = "${src}/Cargo.lock";

  # Force a fully static binary. nixpkgs' musl Rust defaults to DYNAMIC linking
  # (interpreter + libc.so/libgcc_s.so.1 in /nix/store) -- unusable in the guest,
  # which has no /nix/store. The cargoSetupHook generates a per-target
  # [target.<triple>].rustflags with "-Ctarget-feature=-crt-static" (the minus
  # because a plain musl cross isn't isStatic). RUSTFLAGS env outranks that
  # target config in cargo's precedence, so we restate the flags with crt-static
  # flipped on; the hook's separate "linker" key still selects the cross linker.
  RUSTFLAGS = "-Ctarget-feature=+crt-static -Cforce-frame-pointers=yes";

  # Drop the embedded-toolchains linker config; nix supplies the cross linker.
  postPatch = ''
    rm -f .cargo/config .cargo/config.toml
  '';

  # Guest binary -- no host-runnable tests during a cross build.
  doCheck = false;

  meta.mainProgram = binName;
}
