#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Regression tests for packaged-smoke process and mount cleanup."""

import contextlib
import importlib.util
import io
import os
import select
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "smoke", Path(__file__).resolve().parents[1] / "smoke-packaged-runtime.py"
)
smoke = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(smoke)


class CleanupTests(unittest.TestCase):
    def test_functional_error_survives_cleanup_failure(self):
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(smoke.http.server, "ThreadingHTTPServer"),
            mock.patch.object(smoke.subprocess, "Popen"),
            mock.patch.object(
                smoke,
                "wait_for",
                side_effect=[None, RuntimeError("functional failure")],
            ),
            mock.patch.object(
                smoke, "stop", side_effect=[RuntimeError("cleanup failure"), None]
            ) as stop,
            contextlib.redirect_stderr(io.StringIO()) as diagnostics,
        ):
            root = Path(directory)
            args = mock.Mock(prefix=root, cli=root, library_dir=root, appimage=False)
            with self.assertRaisesRegex(RuntimeError, "functional failure"):
                smoke.run(args, root)
            self.assertEqual(stop.call_count, 2)
            self.assertIn("cleanup failure", diagnostics.getvalue())

    def test_rejects_a_recycled_descendant_pid(self):
        process = mock.Mock(pid=10)
        process.poll.return_value = None
        with (
            contextlib.ExitStack() as stack,
            mock.patch.object(smoke, "child_pids", return_value={20}),
            mock.patch.object(os, "pidfd_open", side_effect=[100, 200]),
            mock.patch.object(os, "close") as close,
            mock.patch.object(Path, "read_text", return_value="20 (unrelated) S 99 0"),
        ):
            self.assertEqual(smoke.capture_processes(process, stack), [100])
            stack.close()
            close.assert_has_calls([mock.call(200), mock.call(100)])

    def test_waits_for_descendant_cleanup_after_parent_exits(self):
        child_code = """
import os, signal, sys, time
from pathlib import Path
signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM})
print(os.getpid(), flush=True)
sys.stdin.read()
time.sleep(0.3)
Path(sys.argv[1]).write_text('unmounted')
"""
        parent_code = """
import signal, subprocess, sys
child = subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         start_new_session=True)
print(child.stdout.readline().decode().strip(), flush=True)
signal.pause()
"""
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "cleanup-finished"
            process = subprocess.Popen(
                [sys.executable, "-c", parent_code, child_code, str(marker)],
                stdout=subprocess.PIPE,
                start_new_session=True,
            )
            child_fd = None
            try:
                self.assertTrue(select.select([process.stdout], [], [], 5)[0])
                child_pid = int(process.stdout.readline())
                child_fd = os.pidfd_open(child_pid)
                smoke.stop(process)
                self.assertEqual(marker.read_text(), "unmounted")
            finally:
                if child_fd is not None:
                    with contextlib.suppress(ProcessLookupError):
                        signal.pidfd_send_signal(child_fd, signal.SIGKILL)
                    os.close(child_fd)
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
                process.stdout.close()


class MountTests(unittest.TestCase):
    def setUp(self):
        self.runtime = Path("/tmp/limux smoke/runtime")

    def mount(self, name="doc", kind="fuse.portal", source="portal", uid=None):
        return (
            self.runtime / name,
            kind,
            source,
            [f"user_id={os.getuid() if uid is None else uid}"],
        )

    def test_reads_mountinfo_without_traversing_disconnected_mounts(self):
        mountinfo = (
            "100 1 0:1 / /run/user/1000/doc rw - fuse.portal portal rw,user_id=1000\n"
            "101 1 0:2 / /tmp/limux\\040smoke/runtime/doc rw - fuse.portal portal rw,user_id=1000\n"
            "102 1 0:3 / /tmp/limux\\040smoke/runtime-other/doc rw - fuse.portal portal rw,user_id=1000\n"
        )
        with (
            mock.patch.object(Path, "read_text", return_value=mountinfo),
            mock.patch.object(Path, "stat", side_effect=OSError(107, "disconnected")),
        ):
            self.assertEqual(
                smoke.mounts_under(self.runtime),
                [
                    (
                        self.runtime / "doc",
                        "fuse.portal",
                        "portal",
                        ["rw", "user_id=1000"],
                    )
                ],
            )

    def test_waits_for_natural_unmount_after_process_exit(self):
        process = mock.Mock()
        process.poll.return_value = 0
        with (
            mock.patch.object(
                smoke, "mounts_under", side_effect=[[self.mount()], [self.mount()], []]
            ),
            mock.patch.object(smoke.time, "sleep"),
            mock.patch.object(smoke, "detach_runtime_mounts") as detach,
        ):
            smoke.stop(process, self.runtime)
            detach.assert_not_called()

    def test_detaches_only_verified_private_fuse_mounts(self):
        mounts = [self.mount(), self.mount("gvfs", "fuse.gvfsd-fuse", "gvfsd-fuse")]
        with (
            mock.patch.object(smoke, "mounts_under", return_value=mounts),
            mock.patch.object(smoke.subprocess, "run") as unmount,
        ):
            smoke.detach_runtime_mounts(self.runtime)
        self.assertEqual(
            unmount.call_args_list,
            [
                mock.call(
                    ["fusermount3", "-u", "-z", "--", str(self.runtime / name)],
                    check=True,
                    timeout=5,
                )
                for name in ("doc", "gvfs")
            ],
        )

    def test_rejects_unknown_mounts_before_unmounting_anything(self):
        for mount in (
            self.mount("unexpected"),
            self.mount("doc/child"),
            self.mount(kind="fuse.other"),
            self.mount(source="other"),
            self.mount(uid=os.getuid() + 1),
        ):
            with (
                self.subTest(mount=mount),
                mock.patch.object(
                    smoke, "mounts_under", return_value=[self.mount(), mount]
                ),
                mock.patch.object(smoke.subprocess, "run") as unmount,
            ):
                with self.assertRaisesRegex(RuntimeError, "unexpected private mount"):
                    smoke.detach_runtime_mounts(self.runtime)
                unmount.assert_not_called()

    def test_unmount_failure_is_not_ignored(self):
        with (
            mock.patch.object(smoke, "mounts_under", return_value=[self.mount()]),
            mock.patch.object(
                smoke.subprocess,
                "run",
                side_effect=subprocess.CalledProcessError(1, "fusermount3"),
            ),
            self.assertRaises(subprocess.CalledProcessError),
        ):
            smoke.detach_runtime_mounts(self.runtime)

    def test_auto_unmount_winning_the_race_is_success(self):
        with (
            mock.patch.object(smoke, "mounts_under", side_effect=[[self.mount()], []]),
            mock.patch.object(
                smoke.subprocess,
                "run",
                side_effect=subprocess.CalledProcessError(1, "fusermount3"),
            ),
        ):
            smoke.detach_runtime_mounts(self.runtime)

    def test_refuses_to_delete_a_still_mounted_directory(self):
        with (
            mock.patch.object(smoke, "mounts_under", return_value=[self.mount()]),
            mock.patch.object(smoke.shutil, "rmtree") as remove,
        ):
            with self.assertRaisesRegex(RuntimeError, "refusing to delete mounted"):
                smoke.remove_runtime_directory(self.runtime.parent)
            remove.assert_not_called()


if __name__ == "__main__":
    unittest.main()
