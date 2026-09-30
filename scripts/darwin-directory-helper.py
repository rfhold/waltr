#!/usr/bin/env python3
import ctypes
import errno
import json
import os
import stat
import sys


# sys/stdio.h and sys/mount.h in the macOS SDK.
RENAME_EXCL = 0x00000004
MAXPATHLEN = 1024
MFSTYPENAMELEN = 16


class Fsid(ctypes.Structure):
    _fields_ = [("val", ctypes.c_int32 * 2)]


class Statfs(ctypes.Structure):
    _fields_ = [
        ("f_bsize", ctypes.c_uint32),
        ("f_iosize", ctypes.c_int32),
        ("f_blocks", ctypes.c_uint64),
        ("f_bfree", ctypes.c_uint64),
        ("f_bavail", ctypes.c_uint64),
        ("f_files", ctypes.c_uint64),
        ("f_ffree", ctypes.c_uint64),
        ("f_fsid", Fsid),
        ("f_owner", ctypes.c_uint32),
        ("f_type", ctypes.c_uint32),
        ("f_flags", ctypes.c_uint32),
        ("f_fssubtype", ctypes.c_uint32),
        ("f_fstypename", ctypes.c_char * MFSTYPENAMELEN),
        ("f_mntonname", ctypes.c_char * MAXPATHLEN),
        ("f_mntfromname", ctypes.c_char * MAXPATHLEN),
        ("f_flags_ext", ctypes.c_uint32),
        ("f_reserved", ctypes.c_uint32 * 7),
    ]


def fail(error):
    number = error.errno if isinstance(error, OSError) else errno.ESTALE
    print(json.dumps({
        "code": errno.errorcode.get(number, "EUNKNOWN"),
        "errno": number,
        "message": str(error),
    }), file=sys.stderr)
    return 1


def rename_noreplace(source, destination, directory_fd=None):
    libc = ctypes.CDLL(None, use_errno=True)
    renameatx_np = libc.renameatx_np
    renameatx_np.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    renameatx_np.restype = ctypes.c_int
    owned_fd = None
    if directory_fd is None:
        directory_fd = getattr(os, "AT_FDCWD", None)
        if directory_fd is None:
            owned_fd = os.open(".", os.O_RDONLY | os.O_DIRECTORY)
            directory_fd = owned_fd
    try:
        if renameatx_np(directory_fd, os.fsencode(source), directory_fd, os.fsencode(destination), RENAME_EXCL) != 0:
            number = ctypes.get_errno()
            raise OSError(number, os.strerror(number), destination)
    finally:
        if owned_fd is not None:
            os.close(owned_fd)


def remove_if_identity(path, expected_device, expected_inode, directory_fd=None):
    current = os.stat(path, dir_fd=directory_fd, follow_symlinks=False)
    if not stat.S_ISDIR(current.st_mode) or current.st_dev != expected_device or current.st_ino != expected_inode:
        raise OSError(errno.ESTALE, "directory identity changed before removal", path)
    # macOS has no compare-and-remove directory syscall; same single-user limitation as the Linux helper.
    os.rmdir(path, dir_fd=directory_fd)


def mount_id(directory_fd):
    libc = ctypes.CDLL(None, use_errno=True)
    fstatfs = libc.fstatfs
    fstatfs.argtypes = [ctypes.c_int, ctypes.POINTER(Statfs)]
    fstatfs.restype = ctypes.c_int
    result = Statfs()
    if fstatfs(directory_fd, ctypes.byref(result)) != 0:
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number))
    return {
        "fsid": list(result.f_fsid.val),
        "mountpoint": os.fsdecode(result.f_mntonname),
    }


def same_identity(left, right):
    return left.st_dev == right.st_dev and left.st_ino == right.st_ino


def checked_entry(name, directory_fd, expected, kind):
    current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
    if not kind(current.st_mode) or not same_identity(current, expected):
        raise OSError(errno.ESTALE, "entry changed before removal", name)
    return current


def clear_tree_at(directory_fd):
    if not stat.S_ISDIR(os.fstat(directory_fd).st_mode):
        raise OSError(errno.ENOTDIR, "root descriptor is not a directory")
    parent_mount = mount_id(directory_fd)
    for name in os.listdir(directory_fd):
        entry = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if stat.S_ISREG(entry.st_mode) or stat.S_ISLNK(entry.st_mode):
            kind = stat.S_ISREG if stat.S_ISREG(entry.st_mode) else stat.S_ISLNK
            checked_entry(name, directory_fd, entry, kind)
            os.unlink(name, dir_fd=directory_fd)
        elif stat.S_ISDIR(entry.st_mode):
            child_fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory_fd)
            try:
                if not same_identity(os.fstat(child_fd), entry):
                    raise OSError(errno.ESTALE, "directory changed while opening", name)
                if mount_id(child_fd) != parent_mount:
                    raise OSError(errno.EXDEV, "refusing to cross mount boundary", name)
                clear_tree_at(child_fd)
                checked_entry(name, directory_fd, entry, stat.S_ISDIR)
                os.rmdir(name, dir_fd=directory_fd)
            finally:
                os.close(child_fd)
        else:
            raise OSError(errno.EINVAL, "refusing unexpected tree entry", name)


def parse_expected_files(argument):
    if argument is None:
        return None
    try:
        expected = json.loads(argument)
    except (ValueError, TypeError) as error:
        raise OSError(errno.EINVAL, "invalid expected files JSON") from error
    if expected is None:
        return None
    if not isinstance(expected, dict):
        raise OSError(errno.EINVAL, "expected files must be a mapping or null")
    for name, identity in expected.items():
        if (not name or name in (".", "..") or "/" in name or "\0" in name
                or not isinstance(identity, dict)
                or set(identity) != {"device", "inode"}
                or any(type(identity[key]) is not int or identity[key] < 0 for key in ("device", "inode"))):
            raise OSError(errno.EINVAL, "invalid expected file identity", name)
    return expected


def clear_flat_at(directory_fd, expected_files):
    if not stat.S_ISDIR(os.fstat(directory_fd).st_mode):
        raise OSError(errno.ENOTDIR, "root descriptor is not a directory")
    names = os.listdir(directory_fd)
    if expected_files is not None and set(names) != set(expected_files):
        raise OSError(errno.ESTALE, "flat directory entries changed")
    verified = {}
    for name in names:
        entry = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if not stat.S_ISREG(entry.st_mode):
            raise OSError(errno.EINVAL, "refusing unexpected flat entry", name)
        if expected_files is not None:
            identity = expected_files[name]
            if entry.st_dev != identity["device"] or entry.st_ino != identity["inode"]:
                raise OSError(errno.ESTALE, "flat entry changed", name)
        verified[name] = entry
    for name, entry in verified.items():
        checked_entry(name, directory_fd, entry, stat.S_ISREG)
        os.unlink(name, dir_fd=directory_fd)


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
        elif len(sys.argv) == 2 and sys.argv[1] == "mount-id":
            print(json.dumps(mount_id(3)))
        elif len(sys.argv) == 2 and sys.argv[1] == "clear-tree-at":
            clear_tree_at(3)
        elif len(sys.argv) in (2, 3) and sys.argv[1] == "clear-flat-at":
            clear_flat_at(3, parse_expected_files(sys.argv[2] if len(sys.argv) == 3 else None))
        else:
            raise OSError(errno.EINVAL, "invalid helper arguments")
    except (OSError, ValueError, AttributeError, RecursionError) as error:
        return fail(error)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
