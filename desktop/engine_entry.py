"""Entry point for the bundled engine (PyInstaller). Same program as the `photovault` command."""
import multiprocessing
import os
import sys

# A windowed program has no console, so sys.stdout/stderr/stdin can be None. Logging and argparse
# expect real streams, so point them at the null device.
for name, mode in (("stdout", "w"), ("stderr", "w"), ("stdin", "r")):
    if getattr(sys, name) is None:
        setattr(sys, name, open(os.devnull, mode))

from photovault.cli import main  # noqa: E402

if __name__ == "__main__":
    multiprocessing.freeze_support()
    main()
