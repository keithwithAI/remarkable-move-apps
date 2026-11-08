# WPE WebKit runtime environment for browserapp. `R` is this runtime's root; export it before
# sourcing to relocate (defaults to the installed location). Sourced by browser-launch.sh.
: "${R:=/home/root/secondbrain/wpe}"
export R
L="$R/usr/lib/aarch64-linux-gnu"
export LD_LIBRARY_PATH="$L"
# WebKit release builds compile the helper-process dir in — see the /usr/lib symlink the wrapper
# maintains; WEBKIT_EXEC_PATH is honoured by dev builds and harmless otherwise.
export WEBKIT_EXEC_PATH="$L/wpe-webkit-1.1"
export GIO_EXTRA_MODULES="$L/gio/modules"
export LIBGL_DRIVERS_PATH="$L/dri"
# Software EGL: glvnd finds Mesa via this vendor json; surfaceless swrast needs no GPU/DRM node.
export __EGL_VENDOR_LIBRARY_DIRS="$R/usr/share/glvnd/egl_vendor.d"
export EGL_PLATFORM=surfaceless
export LIBGL_ALWAYS_SOFTWARE=1
export WPE_BACKEND_LIBRARY=libWPEBackend-fdo-1.0.so.1
# One painting thread beside the web process's JS/layout thread: on the 2-core device this overlaps
# tile rasterization with script during Kindle's render burst (and speeds page turns).
export WEBKIT_NICOSIA_PAINTING_THREADS=1
export WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1
export FONTCONFIG_PATH="$R/etc/fonts"
export XDG_DATA_DIRS="$R/usr/share"
export SSL_CERT_FILE="$R/etc/ssl/certs/ca-certificates.crt"
export HOME="$R/home"
export TMPDIR="$R/tmp"
export XDG_RUNTIME_DIR="$R/run"
