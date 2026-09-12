"""Exercise the restart soak's actual shell process-tree sampler on Linux."""
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import tempfile
import time
import unittest


@unittest.skipUnless(sys.platform.startswith("linux"), "soak sampling requires Linux /proc")
class SoakProcessTreeTests(unittest.TestCase):
    def test_children_created_by_worker_threads_and_their_descendants(self):
        root = pathlib.Path(__file__).resolve().parents[2]
        script = (root / "scripts/soak.sh").read_text()
        # Execute the production function without launching the long soak or Cargo.
        function = re.search(r"(?ms)^process_tree\(\) \{\n.*?^\}", script).group(0)
        helper = """
import json,os,pathlib,signal,subprocess,sys,threading,time
stop=threading.Event()
signal.signal(signal.SIGTERM, lambda *_: stop.set())
marker=pathlib.Path(sys.argv[1])
def publish(path, value):
    temporary=path.with_suffix(path.suffix+'.tmp')
    temporary.write_text(value)
    temporary.replace(path)
if len(sys.argv)>2:
    grandchild=subprocess.Popen(['/bin/sleep','30'])
    publish(marker,str(grandchild.pid))
    try:
        stop.wait(20)
    finally:
        grandchild.terminate()
        grandchild.wait(timeout=5)
else:
    leader_child=subprocess.Popen(['/bin/sleep','30'])
    def worker():
        child_marker=marker.with_suffix('.child')
        child=subprocess.Popen([sys.executable,'-c',sys.argv[0],str(child_marker),'child'])
        try:
            deadline=time.monotonic()+5
            while not child_marker.exists() and time.monotonic()<deadline:
                time.sleep(.01)
            publish(marker,json.dumps({'parent':os.getpid(),'leader_child':leader_child.pid,
                'worker':threading.get_native_id(),'worker_child':child.pid,
                'grandchild':int(child_marker.read_text())}))
            stop.wait(20)
        finally:
            child.terminate()
            child.wait(timeout=5)
    thread=threading.Thread(target=worker)
    thread.start()
    try:
        stop.wait(20)
    finally:
        stop.set()
        thread.join(timeout=5)
        leader_child.terminate()
        leader_child.wait(timeout=5)
"""
        with tempfile.TemporaryDirectory(prefix="rustydlna-soak-thread-test-") as directory:
            marker = pathlib.Path(directory) / "ready"
            # Pass the helper source in argv[0] so its worker can launch one
            # child that owns a further descendant without another source file.
            parent = subprocess.Popen(
                [sys.executable, "-c", "import sys; source=sys.argv.pop(1); sys.argv[0]=source; exec(source)", helper, str(marker)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                text=True, start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 5
                while not marker.exists() and time.monotonic() < deadline and parent.poll() is None:
                    time.sleep(0.01)
                self.assertTrue(marker.exists(), "threaded test parent did not become ready")
                ids = json.loads(marker.read_text())
                leader = pathlib.Path(f"/proc/{ids['parent']}/task/{ids['parent']}/children").read_text().split()
                worker = pathlib.Path(f"/proc/{ids['parent']}/task/{ids['worker']}/children").read_text().split()
                self.assertNotIn(str(ids["worker_child"]), leader)
                self.assertIn(str(ids["worker_child"]), worker)
                result = subprocess.run(
                    ["/bin/sh", "-c", function + '\nprocess_tree "$1"', "soak-tree-test", str(ids["parent"])],
                    capture_output=True, text=True, check=True, timeout=5,
                )
                sampled = [int(pid) for pid in result.stdout.split()]
                expected = {ids[key] for key in ("parent", "leader_child", "worker_child", "grandchild")}
                self.assertEqual(set(sampled), expected)
                self.assertEqual(len(sampled), len(expected), "a process must be counted only once")
                self.assertNotIn(os.getpid(), sampled, "unrelated sampler parent is outside the tree")
            finally:
                parent.terminate()
                try:
                    _, stderr = parent.communicate(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(parent.pid, signal.SIGKILL)
                    parent.communicate(timeout=5)
                    raise
                self.assertEqual(parent.returncode, 0, stderr)


if __name__ == "__main__":
    unittest.main()
