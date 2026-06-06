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

Once Xiaojin OS is running from SD, upload and jump to a fresh image through the
resident serial loader:

```bash
cargo run -- xload --image ../xiaojin-os/target/aarch64-unknown-none/release/xiaojin-os.bin
```

`xload` defaults to the board profile's `xload_addr`, which is `auto` for R5C.
This is safe only after the OS image becomes position independent or gains a
copy-down trampoline. The current Xiaojin OS image is statically linked at
`0x04080000`, so `xjctl` refuses `auto` with `go` to avoid jumping into the
alternate slot and corrupting runtime state. For now, update the SD image and
boot with `run-sd` when the link-address image must be refreshed.

For the normal edit-build-run loop, use:

```bash
cargo run -- dev
```

This command is temporarily guarded for the R5C static image. The next loader
milestone is either PIC output or a trampoline reload path, after which `dev`
can go back to true one-command hot reload.

Installed CLI equivalent:

```bash
xjctl dev
```

`dev` runs `build-os`, sends the rebuilt image with `xload`, and waits for the
new `xj> ` prompt.

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
