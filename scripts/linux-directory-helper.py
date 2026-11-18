#!/usr/bin/env python3
import ctypes
import errno
import fcntl
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


def promote_install(parent, source, destination, parent_identity, previous):
    directory_fd = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        # Serialize only promotion, not builds. Every cooperating installer
        # validates its original destination snapshot under this same lock.
        fcntl.flock(directory_fd, fcntl.LOCK_EX)
        directory = os.fstat(directory_fd)
        if (directory.st_dev, directory.st_ino) != parent_identity or directory.st_uid != os.geteuid() or directory.st_mode & 0o022:
            raise OSError(errno.ESTALE, "Linux install directory changed during install", parent)
        if not previous:
            rename_noreplace(source, destination, directory_fd)
            return
        try:
            current = os.stat(destination, dir_fd=directory_fd, follow_symlinks=False)
            identity = (current.st_dev, current.st_ino, current.st_ctime_ns, current.st_mtime_ns, current.st_size)
        except FileNotFoundError:
            identity = None
        if identity != previous:
            raise OSError(errno.ESTALE, "Destination changed during build; refusing replacement", destination)
        # A process ignoring flock can still race this check and rename.
        os.replace(source, destination, src_dir_fd=directory_fd, dst_dir_fd=directory_fd)
    finally:
        os.close(directory_fd)


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
        elif len(sys.argv) in (7, 12) and sys.argv[1] == "promote-install":
            parent, source, destination = sys.argv[2:5]
            parent_identity = tuple(map(int, sys.argv[5:7]))
            previous = tuple(map(int, sys.argv[7:]))
            promote_install(parent, source, destination, parent_identity, previous)
        else:
            raise OSError(errno.EINVAL, "invalid helper arguments")
    except (OSError, ValueError, AttributeError) as error:
        return fail(error)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
