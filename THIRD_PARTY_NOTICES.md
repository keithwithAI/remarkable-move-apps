# Third-party notices

The code in this repository is MIT licensed, see [LICENSE](LICENSE). The components below keep their own licenses.

## Fonts and icons

| Component | License | Where |
|---|---|---|
| IBM Plex Sans, Serif and Mono | SIL Open Font License 1.1 | [secondbrain/assets/fonts/OFL-IBM-Plex.txt](secondbrain/assets/fonts/OFL-IBM-Plex.txt) |
| Bootstrap Icons (subset) | MIT | [secondbrain/assets/fonts/bootstrap-icons-LICENSE.txt](secondbrain/assets/fonts/bootstrap-icons-LICENSE.txt) |

## Ported code

The `==highlight==` insertion rules in `secondbrain/crates/highlight` are ported from [reading-highlighter](https://github.com/keithwithAI/reading-highlighter) (MIT).

## Rust dependencies

Release binaries statically include the crates listed in [secondbrain/Cargo.lock](secondbrain/Cargo.lock). Each one is available under a permissive license such as MIT, Apache-2.0, BSD, ISC, Zlib or Unicode-3.0. To see the license for every crate, run `cargo metadata --format-version 1` in `secondbrain/`.

The binaries also include libgit2 (GPL-2.0 with a linking exception), libssh2 (BSD-3-Clause) and OpenSSL 3 (Apache-2.0), built from source through the `libgit2-sys`, `libssh2-sys` and `openssl-src` crates.

## Kindle Browser runtime

The `wpe-runtime-v1` release asset contains unmodified binaries and data files from Debian 12 (bookworm) arm64 packages, including:

| Component | Debian source package | License |
|---|---|---|
| WPE WebKit 2.38.6 | `wpewebkit` | LGPL-2.1 and BSD |
| libsoup 3 | `libsoup3` | LGPL-2.1 |
| GLib | `glib2.0` | LGPL-2.1 |
| Mesa | `mesa` | MIT and others |
| WPEBackend-fdo | `wpebackend-fdo` | BSD-2-Clause |
| Liberation fonts 1.07 | `fonts-liberation` | GPL-2.0 with a font exception |

The full list of packages, with exact versions, is in [secondbrain/deploy/wpe/RUNTIME-PACKAGES.tsv](secondbrain/deploy/wpe/RUNTIME-PACKAGES.tsv). The runtime includes each package's license terms under `usr/share/doc/<package>/copyright`, and the common license texts under `usr/share/common-licenses/`.

`browserapp` links to these libraries dynamically, so you can replace them with your own builds.

### Source code

To get the source for any package in the runtime, on Debian 12 run `apt-get source <package>`, or find that exact version at [snapshot.debian.org](https://snapshot.debian.org/) or [sources.debian.org](https://sources.debian.org/).

For three years from the date of each release that includes this runtime, I will provide the complete corresponding source code for any GPL- or LGPL-licensed package in it, for no more than the cost of supplying it. To request it, open an issue on this repository.

## Not included

XOVI, AppLoad and xovi-tripletap aren't distributed here. You install them from their own projects. Kindle and Kindle Cloud Reader are trademarks of Amazon, and reMarkable is a trademark of reMarkable AS. This project isn't affiliated with either company.
