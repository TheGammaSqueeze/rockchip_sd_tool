# Changelog

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
