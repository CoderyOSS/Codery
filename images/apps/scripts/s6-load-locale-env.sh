#!/bin/bash
# s6-rc longrun services do NOT inherit the container's ENV (Dockerfile ENV or
# `docker run -e`). s6-overlay captures it into /run/s6/container_environment/
# at init, but each service's `run` script must explicitly pull what it needs.
#
# This helper loads ONLY locale-related variables (no secrets) so every app
# gets consistent UTF-8 handling. Required by Elixir/Erlang: without
# ELIXIR_ERL_OPTIONS=+fnu, beam.smp defaults to latin1 filename encoding and
# Elixir warns on every startup.
#
# Source near the top of every s6-rc run script:
#   . /usr/local/bin/s6-load-locale-env

_ce=/run/s6/container_environment
if [ -d "$_ce" ]; then
  for _v in LANG LC_ALL ELIXIR_ERL_OPTIONS LOCALE_ARCHIVE; do
    [ -r "$_ce/$_v" ] && export "$_v=$(cat "$_ce/$_v")"
  done
fi
unset _ce _v
