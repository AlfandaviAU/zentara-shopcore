#!/bin/sh
# Drop-in for the legacy `manage.py <command> [args]`.
exec /usr/local/bin/shopcore "$@"
