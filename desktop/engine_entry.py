"""Entry point for the bundled engine (PyInstaller). Same program as the `photovault` command."""
import multiprocessing

from photovault.cli import main

if __name__ == "__main__":
    multiprocessing.freeze_support()
    main()
