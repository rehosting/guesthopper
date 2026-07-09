# Per-arch build matrix (copied from penguin-tools/src/archs.nix so this repo
# builds itself standalone -- no penguin-tools dependency).
#
# crossSystem      -- glibc cross system (unused here; kept for parity with the
#                     shared matrix so arch additions stay in sync).
# muslCrossSystem  -- musl cross system used to build the fully-static guest
#                     binary (rustPlatform cross + crt-static).
{
  x86_64 = {
    penguinName = "x86_64";
    compatNames = [ "intel64" ];
    crossSystem = {
      config = "x86_64-unknown-linux-gnu";
    };
    muslCrossSystem = {
      config = "x86_64-linux-musl";
    };
  };

  armel = {
    penguinName = "armel";
    crossSystem = {
      config = "armv7l-unknown-linux-gnueabihf";
    };
    muslCrossSystem = {
      config = "armv7l-linux-musleabi";
    };
  };

  arm64 = {
    penguinName = "aarch64";
    crossSystem = {
      config = "aarch64-unknown-linux-gnu";
    };
    muslCrossSystem = {
      config = "aarch64-linux-musl";
    };
  };

  mipsel = {
    penguinName = "mipsel";
    crossSystem = {
      config = "mipsel-unknown-linux-gnu";
      gcc.arch = "mips32r2";
    };
    muslCrossSystem = {
      config = "mipsel-linux-musl";
      gcc.arch = "mips32r2";
    };
  };

  mipseb = {
    penguinName = "mipseb";
    crossSystem = {
      config = "mips-unknown-linux-gnu";
      gcc.arch = "mips32r2";
    };
    muslCrossSystem = {
      config = "mips-linux-musl";
      gcc.arch = "mips32r2";
    };
  };

  mips64el = {
    penguinName = "mips64el";
    crossSystem = {
      config = "mips64el-unknown-linux-gnuabi64";
      gcc.arch = "mips64r2";
      gcc.abi = "64";
    };
    muslCrossSystem = {
      config = "mips64el-linux-musl";
      gcc.arch = "mips64r2";
      gcc.abi = "64";
    };
  };

  mips64eb = {
    penguinName = "mips64eb";
    crossSystem = {
      config = "mips64-unknown-linux-gnuabi64";
      gcc.arch = "mips64r2";
      gcc.abi = "64";
    };
    muslCrossSystem = {
      config = "mips64-linux-musl";
      gcc.arch = "mips64r2";
      gcc.abi = "64";
    };
  };

  ppc64 = {
    penguinName = "powerpc64";
    crossSystem = {
      config = "powerpc64-unknown-linux-gnuabielfv2";
      gcc.abi = "elfv2";
    };
    muslCrossSystem = {
      config = "powerpc64-linux-musl";
      gcc.abi = "elfv2";
    };
  };

  ppc64el = {
    penguinName = "powerpc64le";
    compatNames = [ "powerpc64el" ];
    crossSystem = {
      config = "powerpc64le-unknown-linux-gnu";
    };
    muslCrossSystem = {
      config = "powerpc64le-linux-musl";
    };
  };

  riscv64 = {
    penguinName = "riscv64";
    crossSystem = {
      config = "riscv64-unknown-linux-gnu";
    };
    muslCrossSystem = {
      config = "riscv64-linux-musl";
    };
  };

  loongarch = {
    penguinName = "loongarch64";
    crossSystem = {
      config = "loongarch64-unknown-linux-gnu";
    };
    muslCrossSystem = {
      config = "loongarch64-linux-musl";
    };
  };
}
