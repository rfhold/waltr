#!/usr/bin/env python3
import ctypes
import errno
import json
import os
import re
import sys


PROC_PIDTBSDINFO = 3
MAXCOMLEN = 16


class ProcBsdInfo(ctypes.Structure):
    # sys/proc_info.h: struct proc_bsdinfo (MAXCOMLEN from sys/param.h).
    _fields_ = [
        ("pbi_flags", ctypes.c_uint32),
        ("pbi_status", ctypes.c_uint32),
        ("pbi_xstatus", ctypes.c_uint32),
        ("pbi_pid", ctypes.c_uint32),
        ("pbi_ppid", ctypes.c_uint32),
        ("pbi_uid", ctypes.c_uint32),
        ("pbi_gid", ctypes.c_uint32),
        ("pbi_ruid", ctypes.c_uint32),
        ("pbi_rgid", ctypes.c_uint32),
        ("pbi_svuid", ctypes.c_uint32),
        ("pbi_svgid", ctypes.c_uint32),
        ("rfu_1", ctypes.c_uint32),
        ("pbi_comm", ctypes.c_char * MAXCOMLEN),
        ("pbi_name", ctypes.c_char * (2 * MAXCOMLEN)),
        ("pbi_nfiles", ctypes.c_uint32),
        ("pbi_pgid", ctypes.c_uint32),
        ("pbi_pjobc", ctypes.c_uint32),
        ("e_tdev", ctypes.c_uint32),
        ("e_tpgid", ctypes.c_uint32),
        ("pbi_nice", ctypes.c_int32),
        ("pbi_start_tvsec", ctypes.c_uint64),
        ("pbi_start_tvusec", ctypes.c_uint64),
    ]


def read_bsdinfo(libproc, pid):
    info = ProcBsdInfo()
    ctypes.set_errno(0)
    size = libproc.proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, ctypes.byref(info), ctypes.sizeof(info))
    if size <= 0:
        number = ctypes.get_errno()
        if number == errno.ESRCH:
            return None
        if size == 0 and number == 0:
            # libproc may return zero without setting errno when the PID vanishes.
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return None
            except PermissionError:
                pass
            number = errno.EIO
        raise OSError(number, os.strerror(number))
    if size != ctypes.sizeof(info):
        raise OSError(errno.EIO, f"unexpected proc_bsdinfo size: {size}")
    if info.pbi_pid != pid or not 0 < info.pbi_pgid <= 0x7FFFFFFF:
        raise OSError(errno.ESTALE, "inconsistent process or group ID")
    if not info.pbi_start_tvsec or info.pbi_start_tvusec >= 1000000:
        raise OSError(errno.EIO, "invalid process start time")
    return info


def boot_uuid(libc):
    value = ctypes.create_string_buffer(37)
    length = ctypes.c_size_t(ctypes.sizeof(value))
    if libc.sysctlbyname(b"kern.bootuuid", value, ctypes.byref(length), None, 0) != 0:
        number = ctypes.get_errno()
        raise OSError(number or errno.EIO, "cannot read kern.bootuuid")
    raw = value.raw[:length.value]
    if not re.fullmatch(rb"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\x00", raw):
        raise OSError(errno.EIO, "invalid kern.bootuuid")
    return raw[:-1].decode("ascii").lower()


def identity(pid):
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    libproc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    libproc.proc_pidinfo.restype = ctypes.c_int
    libc = ctypes.CDLL(None, use_errno=True)
    libc.sysctlbyname.argtypes = [ctypes.c_char_p, ctypes.c_void_p, ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p, ctypes.c_size_t]
    libc.sysctlbyname.restype = ctypes.c_int

    first = read_bsdinfo(libproc, pid)
    if first is None:
        return None
    # proc_bsdinfo has no session ID; bracket the separate getsid lookup to
    # reject PID reuse or group/session changes during inspection.
    session_id = os.getsid(pid)
    boot = boot_uuid(libc)
    last = read_bsdinfo(libproc, pid)
    if last is None:
        return None
    if (first.pbi_start_tvsec, first.pbi_start_tvusec, first.pbi_pgid) != (
        last.pbi_start_tvsec, last.pbi_start_tvusec, last.pbi_pgid
    ) or os.getsid(pid) != session_id or not 0 < session_id <= 0x7FFFFFFF:
        raise OSError(errno.ESTALE, "process identity changed during inspection")
    return {
        "pid": pid,
        "processGroupId": last.pbi_pgid,
        "sessionId": session_id,
        "startIdentity": f"{boot}:{last.pbi_start_tvsec}.{last.pbi_start_tvusec:06d}",
    }


def main():
    if sys.platform != "darwin" or len(sys.argv) != 2 or not re.fullmatch(r"[1-9][0-9]{0,9}", sys.argv[1]):
        print("usage: darwin-process-identity.py PID (macOS only)", file=sys.stderr)
        return 2
    pid = int(sys.argv[1])
    if pid > 0x7FFFFFFF:
        print("PID exceeds pid_t range", file=sys.stderr)
        return 2
    try:
        result = identity(pid)
    except ProcessLookupError:
        result = None
    except (OSError, AttributeError) as error:
        print(f"failed to inspect PID {pid}: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
