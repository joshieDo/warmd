#!/bin/sh
set -f
old=$PATH PATH= IFS=:
for dir in $old; do [ "$dir" = "{dir}" ] || PATH=${PATH:+$PATH:}$dir; done
unset IFS
set +f
export PATH
add='{flags}'
nightly='{nightly}'
case $1 in +*) toolchain=$1 ;; *) toolchain= ;; esac
if [ -n "$nightly" ] && "${RUSTC:-rustc}" $toolchain -Zunstable-options -vV >/dev/null 2>&1; then
    add="${add:+$add }$nightly"
fi
if [ -z "$add" ]; then
    :
elif [ -n "${CARGO_ENCODED_RUSTFLAGS+set}" ]; then
    sep=$(printf '\037')
    CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+$CARGO_ENCODED_RUSTFLAGS$sep}$(printf %s "$add" | tr ' ' "$sep")"
    export CARGO_ENCODED_RUSTFLAGS
else
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$add"
    export RUSTFLAGS
fi
exec cargo "$@"
