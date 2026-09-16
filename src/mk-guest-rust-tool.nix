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

  # crates.io returns HTTP 403 to nixpkgs' crate fetcher: it sends a generic
  # `curl/*` User-Agent to the crates.io API download endpoint
  # (https://crates.io/api/v1/crates/<name>/<ver>/download), which crates.io's
  # crawler policy now refuses. The static.crates.io CDN serves the identical
  # bytes (same sha256, so the lockfile checksums still validate) with no UA gate.
  #
  # Point the crate DOWNLOAD at the CDN via importCargoLock's `extraRegistries`,
  # while leaving Cargo.lock CANONICAL (crates stay on the real crates.io-index).
  # Keeping the lock canonical is essential: a rewritten lock source makes
  # `cargo build --frozen` (cargoBuildHook) reject the lock, and this repo is
  # also built against penguin's OLDER nixpkgs via `nixpkgs.follows`, whose
  # importCargoLock/buildRustPackage APIs differ from this repo's newer pin --
  # `cargoLock` + `extraRegistries` is the one crate-source path both accept.
  cargoLock = {
    lockFile = "${src}/Cargo.lock";
    extraRegistries = {
      "https://github.com/rust-lang/crates.io-index" = "https://static.crates.io/crates";
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

  # Drop the embedded-toolchains linker config; it hardcodes /opt/cross paths,
  # and nix supplies the cross linker via CARGO_TARGET_<triple>_LINKER instead.
  postPatch = ''
    rm -f .cargo/config .cargo/config.toml
  '';

  # The `extraRegistries` remap (needed only to send the nix-side crate DOWNLOAD
  # to the CDN) makes importCargoLock's generated vendor config declare a
  # [source."https://github.com/rust-lang/crates.io-index"] block *alongside*
  # cargo's built-in [source.crates-io]; cargo then aborts on the duplicate
  # crates-io definition ("source registry `crates-io` already defined"). The
  # built-in [source.crates-io] replace-with = vendored-sources already vendors
  # every crate (the lock is canonical), so the extra block is pure redundancy --
  # strip it. cargoSetupHook writes the merged config to a `.cargo/config`
  # (older nixpkgs) or `.cargo/config.toml` (newer), and on the older nixpkgs it
  # runs before stdenv cd's into sourceRoot, so the file lands in the BUILD ROOT
  # one level up -- cargo reads it by walking up. Strip both the sourceRoot copy
  # and the parent, both filenames. Runs in preConfigure, after that hook.
  preConfigure = ''
    for cfg in .cargo/config .cargo/config.toml ../.cargo/config ../.cargo/config.toml; do
      [ -f "$cfg" ] || continue
      sed -i '\#^\[source\."https://github\.com/rust-lang/crates\.io-index"\]$#,\#^replace-with#d' "$cfg"
    done
  '';

  # Guest binary -- no host-runnable tests during a cross build.
  doCheck = false;

  meta.mainProgram = binName;
}
