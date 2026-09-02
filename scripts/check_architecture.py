#!/usr/bin/env python3
"""v0.4 architecture gate entry point (mirrors check_v03_architecture.py)."""

if __package__:
    from .check_v04_architecture import main
else:
    from check_v04_architecture import main

if __name__ == "__main__":
    raise SystemExit(main())