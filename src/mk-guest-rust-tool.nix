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

  cargoLock = {
    lockFile = "${src}/Cargo.lock";

    # crates.io now returns HTTP 403 to the generic `curl/*` User-Agent that
    # nixpkgs' crate fetcher sends to its legacy download endpoint on the API
    # host (https://crates.io/api/v1/crates/<name>/<ver>/download). The
    # static.crates.io CDN serves the identical bytes (same sha256, so the
    # Cargo.lock checksums still validate) with no User-Agent gate.
    #
    # Cargo.lock records crates against the *sparse* registry
    # (`sparse+https://index.crates.io/`, cargo's default since 1.70), which
    # importCargoLock's `extraRegistries` hook remaps to the CDN. fetchCrate
    # then builds https://static.crates.io/crates/<name>/<ver>/download. Using
    # the sparse key (rather than the legacy git-index URL) also avoids cargo's
    # "source registry `crates-io` already defined" error, since the git-index
    # URL aliases cargo's built-in crates-io source. This is the exact recipe in
    # nixpkgs' own import-cargo-lock `basic-sparse` test.
    extraRegistries = {
      "sparse+https://index.crates.io/" = "https://static.crates.io/crates";
    };
  };

  # Force a fully static binary. nixpkgs' musl Rust defaults to DYNAMIC linking
  # (interpreter + libc.so/libgcc_s.so.1 in /nix/store) -- unusable in the guest,
  # which has no /nix/store. The cargoSetupHook generates a per-target
  # [target.<triple>].rustflags with "-Ctarget-feature=-crt-static" (the minus
  # because a plain musl cross isn't isStatic). RUSTFLAGS env outranks that
  # target config in cargo's precedence, so we restate the flags with crt-static
  # flipped on; the hook's separate "linker" key still selects the cross linker.
  RUSTFLAGS = "-Ctarget-feature=+crt-static -Cforce-frame-pointers=yes";

  # Drop the embedded-toolchains linker config (it hardcodes /opt/cross paths);
  # nix supplies the cross linker instead. Replace it with a minimal config that
  # pins the built-in crates-io registry to the git protocol. Our Cargo.lock
  # records crates against the sparse registry, and importCargoLock's vendor
  # config replaces `sparse+https://index.crates.io/` with the vendored sources.
  # Without this pin cargo treats that sparse URL AND its built-in crates-io as
  # the same registry -- "source registry `crates-io` already defined" -- so we
  # force crates-io to git, leaving the sparse source distinct. This mirrors the
  # nixpkgs import-cargo-lock `basic-sparse` test.
  postPatch = ''
    rm -f .cargo/config .cargo/config.toml
    mkdir -p .cargo
    printf '[registries.crates-io]\nprotocol = "git"\n' > .cargo/config.toml
  '';

  # Guest binary -- no host-runnable tests during a cross build.
  doCheck = false;

  meta.mainProgram = binName;
}
