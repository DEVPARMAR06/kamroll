"""Device layer. The copier only needs this small interface, so tests use a fake."""
from __future__ import annotations

import fnmatch
import posixpath
from dataclasses import dataclass
from datetime import datetime
from typing import AsyncIterator, Protocol


@dataclass
class RemoteFile:
    path: str
    size: int
    created: datetime
    modified: datetime

    @property
    def name(self) -> str:
        return posixpath.basename(self.path)


class Device(Protocol):
    def list_files(self, roots: list[str], ignore: list[str]) -> AsyncIterator[RemoteFile]: ...
    def read_chunks(self, path: str, offset: int, chunk: int) -> AsyncIterator[bytes]: ...
    async def read_range(self, path: str, offset: int, n: int) -> bytes: ...
    async def reconnect(self) -> None: ...


def ignored(name: str, patterns: list[str]) -> bool:
    return any(fnmatch.fnmatch(name, p) for p in patterns)


class AfcDevice:
    """iPhone over USB via pymobiledevice3 (read-only AFC). Never writes to the phone."""

    def __init__(self) -> None:
        self._ld = None
        self._afc = None
        self._rh = None  # (path, handle) kept open between read_range calls

    async def reconnect(self) -> None:
        from pymobiledevice3.lockdown import create_using_usbmux
        from pymobiledevice3.services.afc import AfcService

        self._rh = None
        self._ld = await create_using_usbmux()
        self._afc = AfcService(self._ld)

    async def _ensure(self):
        if self._afc is None:
            await self.reconnect()
        return self._afc

    async def list_files(self, roots, ignore):
        afc = await self._ensure()
        for root in roots:
            for folder in sorted(await afc.listdir(root)):
                if folder in (".", "..") or ignored(folder, ignore):
                    continue
                fpath = posixpath.join(root, folder)
                st = await afc.stat(fpath)
                if "DIR" not in str(st.get("st_ifmt", "")):
                    entries = [("", fpath)]  # a loose file directly under the root
                else:
                    entries = [(n, posixpath.join(fpath, n)) for n in await afc.listdir(fpath)]
                for n, p in entries:
                    base = n or folder
                    if base in (".", "..") or ignored(base, ignore):
                        continue
                    s = await afc.stat(p)
                    if "REG" not in str(s.get("st_ifmt", "")):
                        continue
                    modified = s["st_mtime"]
                    created = s.get("st_birthtime") or modified
                    yield RemoteFile(p, int(s["st_size"]), created, modified)

    async def read_range(self, path, offset, n):
        """Read n bytes at offset: peeks at a video's header without copying the file."""
        afc = await self._ensure()
        if self._rh is None or self._rh[0] != path:
            if self._rh is not None:
                try:
                    await afc.fclose(self._rh[1])
                except Exception:  # noqa: BLE001
                    pass
            self._rh = (path, await afc.fopen(path, "r"))
        return await afc.pread(self._rh[1], n, offset)

    async def read_chunks(self, path, offset, chunk):
        afc = await self._ensure()
        handle = await afc.fopen(path, "r")
        try:
            pos = offset
            while True:
                data = await afc.pread(handle, chunk, pos)
                if not data:
                    return
                yield data
                pos += len(data)
        finally:
            try:
                await afc.fclose(handle)
            except Exception:
                pass
