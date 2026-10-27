# Handed to CMake when the opus crate builds libopus (see .cargo/config.toml).
#
# The app links the C runtime statically (+crt-static), so it runs on machines
# without the Visual C++ Redistributable. libopus's own CMakeLists forces the
# DLL runtime unless OPUS_STATIC_RUNTIME is on, and the two cannot be mixed in
# one executable (the link fails on __imp_floor and friends).
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "Link libopus against the static MSVC runtime" FORCE)
