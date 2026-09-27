# Changelog

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
