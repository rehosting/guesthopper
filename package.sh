#!/bin/bash
set -eux

echo "Building for 5 target arches"
cargo build --target x86_64-unknown-linux-musl --release
cargo build --target arm-unknown-linux-musleabi  --release
rustup target add aarch64-unknown-linux-musl # this shouldn't be necessary, but seems to be
rustup target add powerpc64-unknown-linux-gnu
rustup target add loongarch64-unknown-linux-gnu
rustup target add riscv64gc-unknown-linux-gnu
cargo build --target aarch64-unknown-linux-musl  --release
RUSTFLAGS='-C target-feature=+crt-static' cargo build --target mips-unknown-linux-musl --release
RUSTFLAGS='-C target-feature=+crt-static' cargo build --target mipsel-unknown-linux-musl --release
RUSTFLAGS='-C target-feature=+crt-static -C linker=powerpc64-linux-gnu-gcc' cargo build --target powerpc64-unknown-linux-gnu --release
RUSTFLAGS='-C target-feature=+crt-static -C linker=loongarch64-unknown-linux-gnu-gcc' cargo build --target loongarch64-unknown-linux-gnu --release
RUSTFLAGS='-C target-feature=+crt-static -C linker=riscv64-linux-gnu-gcc' cargo build --target riscv64gc-unknown-linux-gnu --release

OUT=guesthopper
echo "Packaging into ${OUT} and packaging as guesthopper.tar.gz"
rm  -rf $OUT
mkdir -p $OUT

git config --global --add safe.directory /app
echo "guesthopper $(git rev-parse HEAD) built at $(date)" > $OUT/README.txt

for x in target/*/release/guesthopper; do
  ARCH=$(basename $(dirname $(dirname $x)))
  case $ARCH in
    x86_64-unknown-linux-musl)
      SUFFIX="intel64" #not my favorite name, but penguin-compatible
      ;;
    arm-unknown-linux-musleabi)
      SUFFIX="armel"
      ;;
    aarch64-unknown-linux-musl)
      SUFFIX="aarch64"
      ;;
    mips-unknown-linux-musl)
      SUFFIX="mipseb"
      ;;
    mipsel-unknown-linux-musl)
      SUFFIX="mipsel"
      ;;
    powerpc64-unknown-linux-gnu)
      SUFFIX="powerpc64"
      ;;
    loongarch64-unknown-linux-gnu)
      SUFFIX="loongarch64"
      ;;
    riscv64gc-unknown-linux-gnu)
      SUFFIX="riscv64"
      ;;
      *)
      echo "Unsupported architecture: $ARCH"
      exit 1
      ;;
  esac
  cp $x ${OUT}/guesthopper.${SUFFIX}
  if [ "$SUFFIX" == "mipseb" ]; then
    cp $x ${OUT}/guesthopper.mips64eb
  elif [ "$SUFFIX" == "mipsel" ]; then
    cp $x ${OUT}/guesthopper.mips64el
  fi
done

cp guest_cmd.py ${OUT}/

tar cvfz guesthopper.tar.gz ${OUT}/
