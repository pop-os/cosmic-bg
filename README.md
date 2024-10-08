# cosmic-bg

COSMIC session service which applies backgrounds to displays. Supports the following features:

- Supports common image formats supported by [image-rs](https://github.com/image-rs/image#supported-image-formats)
- 8 and 10-bit background surface layers
- Use of colors and gradients for backgrounds
- Per-display background application
- Wallpaper slideshows that alternate between backgrounds periodically


## Dependencies

Developers should install Rust from from https://rustup.rs/.

- just
- cargo / rustc
- libwayland-dev
- libxkbcommon-dev
- mold
- pkg-config

### Install

A release build can be generated by running `just`, and then installed with `sudo just install`.

If packaging, use the `rootdir` variable to change the root path, in addition to the prefix: `just rootdir=debian/cosmic-bg prefix=/usr install`.

To reduce compile times across COSMIC applications, either use `sccache`, or set `CARGO_TARGET_DIR` to a shared path and install with `sudo -E just install`.

## Debugging

To get debug logs from the service, first kill the `cosmic-bg` process a few times in a row to prevent it from being launched by `cosmic-session`. Then launch it with `just run` to display backtraces and debug logs in the terminal.

## License

Licensed under the [Mozilla Public License Version 2.0](https://choosealicense.com/licenses/mpl-2.0).

### Contribution

Any contribution intentionally submitted for inclusion in the work by you shall be licensed under the Mozilla Public License Version 2.0 (MPL-2.0). Each source file should have a SPDX copyright notice at the top of the file:

```
// SPDX-License-Identifier: MPL-2.0
```
