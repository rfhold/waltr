#!/usr/bin/env python3
import ctypes
import errno
import json
import os
import stat
import sys


AT_FDCWD = -100
RENAME_NOREPLACE = 1


def fail(error):
    number = error.errno if isinstance(error, OSError) else errno.ESTALE
    print(json.dumps({
        "code": errno.errorcode.get(number, "EUNKNOWN"),
        "errno": number,
        "message": str(error),
    }), file=sys.stderr)
    return 1


def rename_noreplace(source, destination, directory_fd=AT_FDCWD):
    libc = ctypes.CDLL(None, use_errno=True)
    renameat2 = libc.renameat2
    renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    renameat2.restype = ctypes.c_int
    if renameat2(directory_fd, os.fsencode(source), directory_fd, os.fsencode(destination), RENAME_NOREPLACE) != 0:
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number), destination)


def remove_if_identity(path, expected_device, expected_inode, directory_fd=None):
    current = os.stat(path, dir_fd=directory_fd, follow_symlinks=False)
    if not stat.S_ISDIR(current.st_mode) or current.st_dev != expected_device or current.st_ino != expected_inode:
        raise OSError(errno.ESTALE, "directory identity changed before removal", path)
    # Linux has no compare-and-remove directory syscall. This final check and rmdir
    # are sufficient for Waltr's single-user local harness, but not a hostile same-UID mutator.
    os.rmdir(path, dir_fd=directory_fd)


def main():
    try:
        if len(sys.argv) == 4 and sys.argv[1] == "rename-noreplace":
            rename_noreplace(sys.argv[2], sys.argv[3])
        elif len(sys.argv) == 4 and sys.argv[1] == "rename-noreplace-at":
            rename_noreplace(sys.argv[2], sys.argv[3], 3)
        elif len(sys.argv) == 5 and sys.argv[1] == "remove-if-identity":
            remove_if_identity(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]))
        elif len(sys.argv) == 5 and sys.argv[1] == "remove-if-identity-at":
            remove_if_identity(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), 3)
        else:
            raise OSError(errno.EINVAL, "invalid helper arguments")
    except (OSError, ValueError, AttributeError) as error:
        return fail(error)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
