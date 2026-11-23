#!/bin/sh
# Compatibility shim: there is no Python in this image, but existing runbooks
# call `python manage.py <command>` and `python run.py`. Map those to the
# Rust binary; anything else is refused.
case "$1" in
  manage.py|./manage.py|*/manage.py) shift; exec /usr/local/bin/shopcore "$@" ;;
  run.py|./run.py|*/run.py) exec /usr/local/bin/shopcore serve ;;
  *) echo "python is not installed; use: shopcore <command>" >&2; exit 127 ;;
esac
