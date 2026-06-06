# Xiaojin Lab

Host-side control tools for Xiaojin OS devices.

`xiaojin-os` runs on the target board. `xjctl` runs on the Mac and manages
serial logs, U-Boot, SD images, and automated run checks.

The default R5C board profile is embedded in the binary, so installed `xjctl`
commands can be run from any working directory. A local `boards/r5c.toml` still
overrides that default when present.

Default logs are written to this project's `logs/` directory, even when `xjctl`
is launched from another working directory.

## Build

```bash
cargo build --release
```

## Typical R5C Flow

Build the OS:

```bash
cargo run -- build-os --os-dir ../xiaojin-os
```

Copy the image to a mounted FAT32 SD card:

```bash
cargo run -- sd-copy --volume /Volumes/R5COS --image ../xiaojin-os/target/aarch64-unknown-none/release/xiaojin-os.bin
```

Break into U-Boot and run the image from SD:

```bash
cargo run -- run-sd --arm-after-enter
```

Probe U-Boot loading capabilities:

```bash
cargo run -- probe-uboot --arm-after-enter
```

Upload and run the image over the serial console, without touching the SD card:

```bash
cargo run -- run-serial --arm-after-enter
```

Show latest log:

```bash
cargo run -- logs latest
```

Run the local web console:

```bash
cargo run -- serve
```

Then open:

```text
http://127.0.0.1:8795
```
