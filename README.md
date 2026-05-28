# Debian Preseed USB Installer Builder

This is a Rust application that automates the process of creating a fully preseeded, unattended Debian installation USB drive.

It performs the following steps:
1. **Downloads** the latest Debian offline `DVD-1` ISO (or uses a provided local ISO).
2. **Injects** your `preseed.cfg` directly into the ISO's `initrd.gz`. This ensures the Debian installer automatically detects and uses it without requiring manual boot parameter modifications.
3. **Flashes** the customized ISO directly to a target USB block device.

## Dependencies

- **Rust / Cargo** (to build the tool)
- **`xorriso`** (required for extracting and repacking the ISO)
- **Root/Administrator privileges** (required to write to a USB block device)

### Why `xorriso` instead of native Rust crates?

`xorriso` remains a strict system dependency.

Native Rust crates like `hadris-iso` and `isobemak` were thoroughly evaluated, but they lack the ability to effectively "copy-on-write" or stream from an existing ISO. Using those native crates would require fully unpacking the 3.8GB Debian DVD to the host's temporary disk, patching the boot files, and then rebuilding a new 3.8GB ISO from scratch (which doubles disk I/O and temporary storage requirements). Furthermore, Debian uses a highly complex hybrid boot catalog (ISOLINUX for BIOS, GRUB for UEFI, and a Hybrid MBR for USB) that is extremely brittle to reconstruct from scratch.

`xorriso` was built for this exact edge case. By using the `-boot_image any replay` flag combined with `-map`, `xorriso` flawlessly clones the complex hybrid boot structure from the original ISO and perfectly streams the 3.8GB of existing packages directly into the new image, dynamically swapping in our modified files on the fly. This prevents excessive disk usage, memory exhaustion, and bootloader corruption.

### Installing Dependencies

- **Debian / Ubuntu:** 
  ```bash
  sudo apt update
  sudo apt install xorriso
  ```
- **macOS:** 
  ```bash
  brew install xorriso
  ```
- **Windows:** Supported, but raw block device access requires opening the `\\.\PhysicalDriveX` path with Administrator privileges, and `xorriso` must be available in your PATH.

## Usage

**WARNING: This tool will completely overwrite the target device. Make sure you select the correct USB drive.**

```bash
cargo build --release

# Run with sudo to allow writing to the USB block device (e.g., /dev/sdX, /dev/diskN)
sudo ./target/release/debian_installer_builder --device /dev/sdX --preseed /path/to/your/preseed.cfg
```

### Options

```text
Usage: debian_installer_builder [OPTIONS] --device <DEVICE> --preseed <PRESEED>

Options:
  -d, --device <DEVICE>    Target USB block device (e.g., /dev/sdX on Linux)
  -p, --preseed <PRESEED>  Path to the preseed file
  -u, --url <URL>          URL to the Debian DVD ISO. If omitted, downloads the latest stable release.
  -i, --iso <ISO>          Optional path to a pre-downloaded ISO (skips download if provided)
      --username <USERNAME> Override the username in the preseed file
      --password <PASSWORD> Override the password in the preseed file
      --hostname <HOSTNAME> Override the hostname in the preseed file
  -h, --help               Print help
  -V, --version            Print version
```

## Cross-Platform Support

This tool is written in pure Rust with minimal external dependencies. However, because it manipulates ISO9660 filesystems and El Torito boot records, it delegates ISO repackaging to `xorriso`, which is the industry standard for Debian hybrid ISOs. 

- **Linux:** Fully supported (`/dev/sdX` block devices).
- **macOS:** Supported (`/dev/diskN` block devices), provided `xorriso` is installed via Homebrew.
- **Windows:** Supported, but raw block device access requires opening the `\\.\PhysicalDriveX` path with Administrator privileges, and `xorriso` must be available in your PATH.

## Integration Testing

This project includes a comprehensive, end-to-end integration test that verifies the entire installer pipeline without needing physical hardware.

The integration test performs the following steps automatically:
1. Runs `debian_installer_builder` to build a custom Debian ISO and writes it to a temporary mock USB image file.
2. Creates a blank QEMU virtual hard drive (`qcow2`).
3. Boots a QEMU virtual machine using the customized mock USB image to simulate an unattended installation onto the virtual hard drive.
4. Reboots the VM from the newly installed virtual hard drive.
5. Verifies the installation by establishing an SSH connection using the injected credentials (`--username` and `--password`).

### Running the Integration Test

Because a full Debian installation takes 5-15 minutes (depending on your host and disk speed), this test requires patience.

**Test Prerequisites (Linux):**
```bash
sudo apt install qemu-system-x86 qemu-utils sshpass
```

**Execute the Test:**
```bash
cargo test -- --nocapture
```
*(The `--nocapture` flag allows you to see the progress of the `debian_installer_builder` build and SSH polling in real-time).*

## Development Notes & Learnings

During the development and testing of this tool, several important nuances about Debian ISOs and `xorriso` were discovered and addressed:

1. **Repacking with Xorriso:** `xorriso` is strict about overwriting existing files. When using `-outdev` to create the new ISO, if the file already exists it will throw a `FAILURE : -indev differs from -outdev and -outdev media holds non-zero data`. The tool resolves this by explicitly removing the target ISO before repacking (or using `-overwrite on`).
2. **Bypassing the Boot Menu:** A standard Debian ISO boots into a menu with `timeout 0` (meaning it waits infinitely for a user to press Enter). Even if a `preseed.cfg` is injected, the installation will never start unattended. To achieve a truly zero-touch installation, the tool extracts `/isolinux/isolinux.cfg`, `/isolinux/txt.cfg`, and `/boot/grub/grub.cfg` from the ISO. It patches these files to set the timeout to 1 second, sets the default boot option to `install`, and appends `auto=true priority=critical` to the kernel arguments. These modified files are then mapped back into the ISO during the `xorriso` repack using the `-map` flag.
3. **Physical vs. Serial Console:** Debugging a headless VM installation can be extremely difficult. We provide a `--serial-console` flag that patches the bootloader to append `console=ttyS0,115200n8` to the kernel arguments. In integration tests, this allows QEMU to dump the log via `-serial file:log.txt`. However, this is an **opt-in flag** for a reason: the Debian installer's internal `rootskel` package has issues spawning the installer frontend properly on the physical VGA display if *any* `console=ttyS0` parameter is detected (even if you attempt a dual-console `console=ttyS0 console=tty0` configuration). Therefore, for standard physical USB installations, we omit the flag so the installer natively binds to the primary physical display (tty1).
4. **Read-Only Extractions:** `xorriso` extracts files (like the bootloader configs) with read-only permissions. If they need to be modified in place, standard `fs::write` calls will silently fail with permission denied errors. The tool works around this by using `fs::remove_file` on the extracted files before writing the patched configuration.
5. **Offline Installation & Preseed:** To ensure the installation can proceed fully offline without hanging on network setup, the tool uses the offline `DVD-1` image instead of the network install variant. Furthermore, the preseed file explicitly disables network mirrors (`d-i apt-setup/use_mirror boolean false`) so the installer reads packages exclusively from the local disc media.
6. **Tmpfs Exhaustion:** Extracting and repacking large ISOs (like the ~3.8GB DVD image) can rapidly exhaust the default `/tmp` tmpfs on Linux, causing `libburn` to fail with "FATAL : Burn run failed". The tool respects the `TMPDIR` environment variable, which can be configured (e.g., `TMPDIR=target/tmp debian_installer_builder ...`) to ensure temporary files are written to a persistent disk with sufficient capacity.
7. **Dynamic Disk Partitioning:** The Debian installer behaves differently depending on how the installation media is mounted. When a virtual CD-ROM is used, it silently ignores it during partitioning. However, when installed from a USB stick, the installer sees the USB as a standard block device alongside the target SSD/HDD and halts to prompt the user about which disk to format. To ensure a truly unattended installation that dynamically targets the internal drive and ignores the USB stick, we inject a custom `partman/early_command` shell script into the `preseed.cfg`. This script uses `list-devices` to subtract the USB media from the disk list and binds `partman-auto/disk` to the remaining internal drive.
8. **Testing UEFI and Block Devices:** To accurately simulate a physical UEFI installation in QEMU, the integration test passes the mock USB image as a standard `virtio` block device rather than a `cdrom`. Furthermore, it utilizes prebuilt OVMF firmware (`code.fd` and `vars.fd`) to force QEMU to boot via UEFI instead of legacy BIOS, ensuring that our `grub.cfg` UEFI modifications and our multi-disk partitioning script are rigorously tested together. To keep the test suite hermetic and cross-platform, the integration test dynamically downloads these firmware binaries from the `rust-osdev/ovmf-prebuilt` repository rather than relying on a static host package.
9. **mDNS Verification via PCAP:** QEMU's default `user` networking (SLIRP) acts as a strict NAT and drops multicast packets (like mDNS). To verify that the guest successfully broadcasts its `.local` hostname on the network without requiring `sudo` privileges for a `TAP` interface, the integration test utilizes QEMU's `filter-dump` feature to capture raw outbound Ethernet frames to a `.pcap` file. The test then directly scans the captured payload to prove the guest successfully transmitted its mDNS advertisements.
10. **Dynamic SSH Port Allocation:** To prevent port collisions when running integration tests in parallel or on CI systems, the test binds a temporary listener on `127.0.0.1:0` to acquire a guaranteed unused ephemeral port from the OS, and dynamically maps QEMU's `hostfwd` to that port for the final verification step.
11. **Offline APT Mirror Restoration (deb822 Gotcha):** Because the `preseed.cfg` explicitly disables network mirrors (`apt-setup/use_mirror false`) to ensure the installation doesn't hang offline, the newly installed system boots with a crippled `/etc/apt/sources.list` that only points to a non-existent local CD-ROM. We fix this by running a `late_command` to dynamically inject the official `deb.debian.org` internet repositories. However, because modern Debian (Trixie+) deprecates `sources.list` in favor of the `deb822` multi-line `.sources` format, we generate a `/etc/apt/sources.list.d/debian.sources` file programmatically. **Crucial Gotcha:** The Debian installer's `debconf` parser intercepts and interprets literal `\n` characters inside preseed strings! Attempting to use a single `printf "line1\nline2\n"` command will fail silently and break the installer script. To safely construct multi-line configuration files from within a `late_command`, one must use a chain of individual `echo "..." >> file` statements.
