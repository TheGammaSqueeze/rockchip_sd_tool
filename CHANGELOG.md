# Changelog

## 1.3.0

- New **update card, keep user data** mode (`--update-card --keep-data`, or the fourth entry in
  the mode list). A plain update card leaves the device wiped, because the firmware's `misc` item
  carries "boot-recovery" and "recovery\n--wipe_all" and the recovery writes that item onto the
  device as it is. The new mode clears the two bootloader control blocks inside the `misc` item of
  the copy carried on the card and recomputes the image's trailing MD5, so the recovery still
  accepts the firmware, installs every partition as before, and the device comes back up with its
  user data. Nothing else in the firmware is changed.
  Only suitable when the new firmware can read the data already on the device.

## 1.2.0

- New **firmware update card** mode (`--update-card`, or the mode list in the window), which is
  Rockchip's "Upgrade Firmware" option. The card is not one the device runs from: boot the device
  from it once and it goes into recovery, installs the firmware the card carries and afterwards
  runs from its own internal storage.
  The front of the card holds the loader and the firmware partitions up to and including
  `recovery`, with `misc` set to "boot-recovery" and "--rk_fwupdate"; the rest is a FAT32
  partition holding the image as `sdupdate.img` with `sd_boot_config.config` and `rksdfw.tag`
  beside it. The bootloader's GPT stays at sector 1 without a protective entry, so an operating
  system sees the data partition through the master boot record while the bootloader still finds
  the firmware partitions.
  Two deliberate differences from Rockchip's tool: the data partition is always FAT32, because
  the recovery on these devices mounts it as `vfat` and cannot read the NTFS that tool switches
  to above 2 GiB; and an image of 4 GiB or more is refused, since FAT32 cannot hold it.
- The write mode is now a list in the window rather than a checkbox, with the boot card, the
  upgrade and the update card side by side.

## 1.1.3

- **A fresh card no longer depends on the first-boot recovery wipe.** The firmware carries no
  image for `metadata`, `cache` or `userdata`, so a card kept whatever those sectors held
  before. Android is meant to be handed blank ones: `metadata` holds the keys `userdata` is
  encrypted with, and the two only work when made together. The vendor arranges that by asking
  recovery to wipe on the first boot, so a device that never reaches recovery, or reaches it
  without a command, was left with filesystems it could not repair and sat in the recovery menu.
  A full write now clears the first 4 MiB of those three partitions, which costs seconds and
  makes the first boot deterministic: Android creates the filesystems itself whether or not the
  recovery wipe runs. An upgrade still leaves all three completely alone.

## 1.1.2

- **A card can no longer get stuck in the recovery menu.** Rockchip firmware ships the `misc`
  partition with the same boot command ("boot-recovery" with "--wipe_all") written twice: at
  offset 0, where Google's bootloader convention puts it, and at 16 KiB, where Rockchip's older
  convention puts it. The bootloader reads whichever one matches the firmware's Android version,
  but Android's recovery only ever clears the copy at offset 0. A device whose bootloader reads
  the 16 KiB copy is therefore sent to recovery on every boot while recovery itself finds no
  command and sits in its menu: a device that never finishes booting and cannot be rescued
  without rewriting the card.
  A full write now keeps the boot command only where this firmware's bootloader reads it, using
  the same rule the bootloader uses (the Android version in the boot image header: offset 0 from
  Android 10, 16 KiB before that), and clears the other copy.
- An upgrade now also clears that unused control block, which rescues a card already stuck in
  the recovery loop without touching user data. It still never writes a boot command, so an
  upgrade cannot ask a device to wipe itself.
- Fixed: upgrading a raw `.img` file skipped every range the plan wanted zeroed, on the
  assumption that the target was blank. That is only true of a file being created for a full
  write, so an upgrade of an image file could leave stale data where a sparse partition expects
  zeros. Upgrades now always write their zero ranges. Cards were not affected.

## 1.1.1

- **Upgrade no longer resets the device.** An upgrade was writing the image's `misc`
  partition, which Rockchip firmware ships with the bootloader command `boot-recovery` and
  the recovery argument `--wipe_all`; the device acted on it at the next boot and wiped the
  user data the upgrade was meant to keep. An upgrade now leaves `misc` alone, along with
  `cache`, `metadata`, `frp`, `swap`, `backup` and `userdata`: state that belongs to the
  device, not to the firmware. A full write is unchanged and still writes them, which is what
  makes a new card set itself up.
- Anyone who upgraded a card with 1.1.0 should use 1.1.1 from now on; 1.1.0 upgrades wiped
  user data on the following boot.

## 1.1.0

- New **Upgrade, keep user data** option (`--upgrade`): re-flash a card that already has this
  layout without losing what is on it. The loader and every partition the image carries are
  replaced; the partition table is kept as it is, nothing is resized, and the partitions the
  image does not carry, `userdata` above all, are untouched. The card's table is checked
  against the image's layout first, and a card that does not match is refused before anything
  is written. Raw `.img` files can be upgraded in place too.

## 1.0.2

- macOS: the program no longer runs a root helper. The raw disk is opened through the
  system `authopen` helper (the standard administrator password dialog), and the image
  is read with the user's own permissions. This fixes "cannot open ...: Operation not
  permitted" on images in Downloads and "flush failed: inappropriate ioctl" on raw disks.
- macOS: the built-in SD card slot of Macs is listed again (it reports as an internal
  device; only the boot disk and non-removable internal drives are hidden now).
- Linux: the root helper could not open its progress file when `fs.protected_regular` is
  enabled (the default on current distributions), which showed up as a permission error
  right after the password prompt. The files now live in a private directory.
- The GPT header is written with a consistent last usable LBA (`total - 34`), so Windows
  and the device no longer rewrite it, and the post-write check validates the table
  structurally. Fixes "verification failed in GPT at byte 528 (sector 1)" on Windows.
- Release zips and macOS bundles carry the version number.

## 1.0.1

- Verify the card through the locked handle before releasing it.

## 1.0.0

- First release: SD Boot card creation from RKFW images on Windows, macOS and Linux with
  per-block read-back and retries, raw and xz image output, GUI and command line.
