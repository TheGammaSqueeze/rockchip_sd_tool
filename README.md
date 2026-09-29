# Rockchip SD Tool

A cross-platform (Windows, macOS, Linux) replacement for Rockchip's Windows-only
SDDiskTool ("SD_Firmware_Tool.exe") in its **SD Boot** mode. It takes a Rockchip
RKFW firmware image (the `update.img` style file made by `rkImageMaker`, for example
`RG_DS_Plus_GammaOS_Core.img`) and turns an SD card into a bootable card with the
same layout SDDiskTool v1.69 produces.

Besides real cards it can write the card image to a plain `.img` or a compressed
`.img.xz` file for an SD card of a given size, which other people can then flash
with any image writer.

The GUI follows the Raspberry Pi Imager flow: choose image, choose storage, write.
A command line interface is included for scripting.

![The main window with a firmware image chosen](assets/screenshots/01-main.png)

What it does:

* writes the SD Boot layout of an RKFW image to a card, byte for byte what SDDiskTool v1.69
  produces (loader, every partition the image carries, GPT);
* **upgrades** a card that already has that layout without touching the partition table or
  user data, so a firmware update keeps saves and settings;
* writes the same card image to a raw `.img` or a compressed `.img.xz` for a card size you
  choose, instead of to a card;
* reads every block back as it writes it and rewrites the ones that come back wrong, then
  verifies the whole card, because cheap and counterfeit cards are common;
* runs on Windows, macOS and Linux from one small self-contained binary.

## Screenshots

| | |
| --- | --- |
| **Choose the card.** Only removable disks are listed; the system disk is never selectable. <br> ![Choosing the card](assets/screenshots/02-choose-card.png) | **Or write to a file.** Pick the size of the card the image is meant for and write a `.img` or `.img.xz`. <br> ![Writing to an image file](assets/screenshots/03-image-file.png) |
| **Confirm.** A full write says plainly that the card will be erased. <br> ![Confirming a full write](assets/screenshots/04-confirm.png) | **Writing.** Progress per partition, with the speed and the number of blocks that had to be rewritten. <br> ![Writing in progress](assets/screenshots/05-writing.png) |
| **Done.** The card is verified before it is released. <br> ![Write finished](assets/screenshots/06-done.png) | **Upgrade instead.** With *Upgrade, keep user data* ticked, the same button re-flashes the partitions and keeps everything else. <br> ![Confirming an upgrade](assets/screenshots/07-upgrade-confirm.png) |
| **Upgrade done.** The partition table and user data are still there. <br> ![Upgrade finished](assets/screenshots/08-upgrade-done.png) | |

## Download and build

Prebuilt binaries for Windows, macOS (Intel and Apple silicon) and Linux (x86_64 and
aarch64) are produced by the GitHub Actions workflow in this repository (see the
Releases page or the Actions artifacts).

Building from source needs only a Rust toolchain (https://rustup.rs) and a C compiler
(for the bundled xz library):

```
git clone https://github.com/TheGammaSqueeze/rockchip_sd_tool
cd rockchip_sd_tool
cargo build --release
```

The binary is `target/release/rockchip_sd_tool` (`rockchip_sd_tool.exe` on Windows).
It is a single self-contained executable. The macOS release zips also contain
`Rockchip SD Tool.app`, a double-clickable bundle around the same binary; since it is
not notarized, the first launch needs a right-click, Open (or `xattr -dr
com.apple.quarantine "Rockchip SD Tool.app"`).

Linux build dependencies (Debian/Ubuntu): `sudo apt install build-essential libxkbcommon-dev libwayland-dev libgl1-mesa-dev`.
Windows: Visual Studio Build Tools (C++), which rustup installs on request.
macOS: Xcode command line tools (`xcode-select --install`).

## Usage

### GUI

Run `rockchip_sd_tool` with no arguments (double-click the executable). You can also
pass the image path as the only argument, or drag an image file onto the window.

1. **Choose image**: pick the `.img` RKFW firmware. The tool shows the device model,
   chip, version and the minimum card size the layout needs.
2. **Choose storage**: pick the SD card (only removable USB/SD disks are listed;
   internal disks are hidden unless you ask for them and the system disk is never
   selectable). Or switch to *Image file* and write to a `.img` / `.img.xz` for a
   card of a chosen size. See the screenshots above.
3. **Write**. Tick **Upgrade, keep user data** first if you are re-flashing a card that
   already runs this firmware and want to keep what is on it (see below). On Linux you are
   asked for your password (a root helper does the raw disk
   writing); on macOS the system asks for the administrator password to open the disk,
   like Raspberry Pi Imager, and the program itself keeps running as you; on Windows the
   program asks for administrator rights when it starts.

Every block is flushed to the card, read back and compared right after it is
written; a block that reads back wrong is rewritten, up to three attempts, after
which the write fails with the sector number (bad or counterfeit cards). The
"Verify after writing" option adds a second full read-back pass at the end.

### Upgrading a card without losing user data

**Upgrade, keep user data** (`--upgrade` on the command line) re-flashes a card that already
has this layout: the loader and the firmware partitions (`uboot`, `dtbo`, `vbmeta`, `boot`,
`recovery`, `baseparameter`, `super`) are replaced, and everything else is left exactly as it
is. The partition table is not rewritten and nothing is resized.

These partitions are never written by an upgrade, even when the image carries them:
`misc`, `cache`, `metadata`, `frp`, `swap`, `backup` and `userdata`. They hold the state of the
device rather than firmware, and writing them is what a factory flash does:

* `misc` is the bootloader control block. Rockchip firmware ships it with the command
  `boot-recovery` and the recovery argument `--wipe_all`, which is how a freshly written card
  wipes itself on its first boot. Giving that to a card that is already in use erases exactly
  what the upgrade is meant to keep.
* `metadata` holds the keys user data is encrypted with, so replacing it makes the existing user
  data unreadable.

A full write still writes all of them, because that is what makes a new card set itself up.

Before the first byte is written the card's own partition table is read and compared with the
image's parameter file: every partition must be present, at the same sector, with the same size,
except the growing partition (`userdata`), whose size follows the card it was made on. If
anything differs the write is refused and the card is left untouched, with a message naming the
partition that does not match. Write such a card in full instead.

A raw `.img` file can be upgraded in place the same way; a compressed `.img.xz` cannot.

The screenshots above show the confirmation and the result of an upgrade.

### Command line

```
rockchip_sd_tool info  <image.img> [--size 128GB] [--md5] [--parameter]
rockchip_sd_tool list  [--all]
rockchip_sd_tool write <image.img> --to /dev/sdX            (Linux, needs sudo)
rockchip_sd_tool write <image.img> --to /dev/rdisk4          (macOS, needs sudo)
rockchip_sd_tool write <image.img> --to \\.\PhysicalDrive2   (Windows, admin prompt)
rockchip_sd_tool write <image.img> --to card.img.xz --size 128GB
rockchip_sd_tool write <image.img> --to card.img --size 250347520s
rockchip_sd_tool write <image.img> --to /dev/sdX --upgrade    (keep the table and user data)
rockchip_sd_tool verify <image.img> --from /dev/sdX
```

Sizes accept `G`/`GiB` (binary), `GB` (decimal, what card vendors print), plain bytes,
or `s` for 512-byte sectors (`blockdev --getsz`, `diskutil info`, or Disk Management
give the exact card size). A `.img` is written as a sparse file of the full card size.
A `.img.xz` is a normal single-stream xz file (`xz -dc card.img.xz | dd of=/dev/sdX`
or any imager that reads xz works); it is written block by block, the compressed form
of an all-zero block is reused for every zero block, so a 128 GB card image takes about
as long as compressing the firmware itself, and the block index lets `verify` and the
post-write verification seek instead of decoding the whole file.

Flags: `--upgrade` keeps the partition table and user data (above), `--no-verify` skips the
final full verification pass, `--no-block-verify` skips the per-block read-back,
`--yes` skips the confirmation for devices,
`--xz-level N` sets the xz preset (default 3). On Linux the `write` command needs
`sudo` for a device; on macOS it asks for the administrator password itself.

## What gets written (the SDDiskTool SD Boot layout)

The layout was reverse-engineered from SDDiskTool v1.69 and checked byte for byte
against a card it produced (an RG DS Plus running from that card).

| Where (512-byte sectors) | Content |
| --- | --- |
| 0..2 | zeroed first ("Clear MBR"), then the protective MBR |
| 1..34 | primary GPT header and 128 entries |
| 64 | loader: `FlashHead` (RKNS id block header, 4 sectors) |
| 68 | loader: `FlashData` (DDR init), padded to 4 sectors |
| 68 + data | loader: `FlashBoot` (miniloader), padded to 4 sectors |
| partition offsets from `parameter.txt` | every firmware item that has a partition address, in table order; Android sparse images are expanded (the unpacked extent and the last 64 sectors of the partition are zeroed first, DONT_CARE chunks stay zero) |
| total - 33 | backup GPT entries and header |

Details that matter for a byte-identical result:

* The loader entries inside the RKFW's `BOOT`/`LDR ` blob are RC4 scrambled with
  Rockchip's fixed key (`7c 4e 03 04 55 05 09 07 2d 2c 7b 38 17 0d 17 11`), the
  keystream restarting every 512 bytes. When the blob's rc4 flag is set the boot
  ROM wants plain data, so each full sector is descrambled; a trailing partial sector
  stays as is.
* The `parameter` item is not written for GPT layouts; the GPT is built from its
  `mtdparts` line. `misc` is written verbatim (the "boot-recovery / rk_fwupdate"
  injection only happens in SDDiskTool's upgrade-card mode).
* GPT: first usable LBA 34; a `grow` partition ends at `total - 65` on cards of 4 GiB
  and more (`total - 34` below), exactly like SDDiskTool; partition type and unique GUIDs
  are random version 4 UUIDs (a `uuid:name=...` parameter line overrides the unique
  GUID); `:bootable` sets attribute bit 2; the backup header's entry LBA is `total - 33`.
  One deliberate difference: SDDiskTool writes the header's last usable LBA as
  `total - 65` too, which contradicts its own backup entry position; the RG DS Plus
  firmware rewrites it to `total - 34` on first boot and Windows does the same as soon as
  the disk is released. This tool writes `total - 34` from the start, so the header is
  consistent, nothing rewrites it, and the card is byte for byte what the device would
  make of a SDDiskTool card.
* Items over 4 GiB use afptool's extension (high 32 bits of offset and size stored
  behind an `H` marker inside the file name field), which is honoured.

## Tests

`cargo test` builds a small synthetic RKFW image (scrambled loader, sparse super,
several partitions), writes it to a raw `.img` and to a `.img.xz`, reads both back and
checks the loader placement and descrambling, every partition, the sparse expansion,
the primary and backup GPT (CRCs, ranges, attributes, UUID version), the size checks,
cancellation, the per-block verification with retries (fault injection: a flaky block is
rewritten, a permanently bad block fails after three attempts), and the upgrade mode (user data
and the partition table survive, a card with a different layout or no partition table is
refused without being touched).

## Safety

Raw writes destroy everything on the target disk. The disk list hides internal
drives by default and refuses the system disk; all mounted volumes on the chosen
card are unmounted (Windows: locked and dismounted) before writing.
