# MIPS string functions for the frontend

`memcpy.S` and `memset.S` are newlib's MIPS Technologies implementations
(BSD-style licence, header kept), fetched from newlib-cygwin
`newlib/libc/machine/mips/`. musl has no MIPS-optimised versions (only
generic C), and memcpy/memset were ~10 % of PlayStation emulation time.
Built by scripts/build-cores.sh with the prepare-for-store prefetch hint
(as glibc uses on MIPS; the 74Kc's L1 line is 32 bytes, which that code
assumes) into out/cores/libmipsstring.a, linked ahead of musl.
`include/` holds the two tiny headers the sources expect.
