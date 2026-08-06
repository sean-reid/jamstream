# CMake toolchain file for x86_64-pc-windows-msvc, wired up by .cargo/config.toml.
#
# opus declares option(OPUS_STATIC_RUNTIME ... OFF) and then sets
# CMAKE_MSVC_RUNTIME_LIBRARY to MultiThreadedDLL from it, so its objects import
# the CRT while the rest of the exe links it in. opusic-sys never passes the
# option and has no passthrough for one. option() leaves an existing cache
# entry alone, so forcing it here, before the project is read, is the only way
# in. cmake-rs reads this file's path out of the environment.
#
# Nothing else belongs in here. A toolchain file is read again for every
# try_compile CMake runs, so it has to stay free of side effects, and opus is
# the only cmake dependency in the graph.
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "build opus against the static CRT" FORCE)
