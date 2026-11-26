#!/bin/sh
# Shared by stage.sh and release-build.sh, which supply say and die.

# check_release_binaries DIRECTORY [FAILURE_SUFFIX]
check_release_binaries() (
  build_dir=$1
  failure_suffix=${2:-}
  platform=$(uname -s) || die "cannot determine the build platform$failure_suffix"
  suffix=
  reader='strings'
  case "$platform" in
    # The hosted Windows runner provides llvm-strings to inspect .exe files.
    MINGW*|MSYS*|CYGWIN*) suffix=.exe; reader=llvm-strings ;;
  esac

  # Only a build with the rig's kill switch contains the variable it reads.
  kill_needle=BASAL_RIG_KILL_FILE
  # The module entry point retains this feature's marker even if unused test
  # doubles are optimized out.
  support_needle=BASAL_TEST_SUPPORT_ENABLED
  hook_table=$("$reader" "$build_dir/ck-basal$suffix") || die "cannot inspect the built ck-basal$failure_suffix"
  hook=$(printf '%s\n' "$hook_table" | grep -cF -- "$kill_needle" || true)
  [ "$hook" = 0 ] || die "the built ck-basal contains the rig kill switch ($kill_needle)$failure_suffix"
  say "ck-basal carries no rig kill switch ($kill_needle: 0)"

  for bin in ck-basal ck-basal-worker; do
    case "$bin" in
      ck-basal) support_table=$hook_table ;;
      *) support_table=$("$reader" "$build_dir/$bin$suffix") || die "cannot inspect $bin for test support$failure_suffix" ;;
    esac
    support=$(printf '%s\n' "$support_table" | grep -cF -- "$support_needle" || true)
    [ "$support" = 0 ] || die "$bin contains test support ($support_needle)$failure_suffix"
    if [ "$platform" = Darwin ]; then
      # Mach-O OSO entries point to object files on the build machine. Cargo
      # can succeed even when stripping fails, so inspect both binaries.
      symbols=$(nm -a "$build_dir/$bin") || die "cannot inspect $bin's symbols$failure_suffix"
      oso=$(printf '%s\n' "$symbols" | grep -c ' OSO ' || true)
      [ "$oso" = 0 ] || die "$bin still carries $oso debug-map entries: stripping failed$failure_suffix"
    fi
  done
  if [ "$platform" = Darwin ]; then
    say "debug map: 0 entries in each binary"
  fi
  say "test support: 0 markers in each binary"
)
