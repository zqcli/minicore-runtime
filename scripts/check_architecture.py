#!/usr/bin/env python3
"""Compatibility entry point for the authoritative final v0.3 architecture gate."""

if __package__:
    from .check_v03_architecture import main
else:
    from check_v03_architecture import main


if __name__ == "__main__":
    raise SystemExit(main())
