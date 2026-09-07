#!/bin/bash
# s6-import-container-env — source from s6 run scripts.
#
# s6 services get a minimal env (Launchy children inherited the full container
# env — this restores parity). s6-overlay captures docker env into
# /run/s6/container_environment at boot; import every key here, except those
# listed in $S6_IMPORT_SKIP (space-separated) so the sourcing script can apply
# its own overrides afterwards (e.g. opendesign's NODE_OPTIONS).
_ce=/run/s6/container_environment
if [ -d "$_ce" ]; then
  _skip=" ${S6_IMPORT_SKIP:-} "
  for _f in "$_ce"/*; do
    _k="$(basename "$_f")"
    case "$_skip" in
      *" $_k "*) continue ;;
    esac
    export "$_k=$(cat "$_f")"
  done
  unset _skip _f _k
fi
unset _ce
