"""Keep availability probes tied to source package declarations and trust setup."""

import importlib.util
from pathlib import Path
import unittest


REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("clean_build_probe", REPO / "scripts/clean-build-probe.py")
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)


class CleanBuildProbeTests(unittest.TestCase):
    def test_source_package_changes_reach_each_fresh_download_stage(self):
        source = (REPO / "Dockerfile").read_text()
        rendered = PROBE.probe_dockerfile(source)
        self.assertEqual(rendered.count("apt-get install --download-only"), 3)
        self.assertEqual(rendered.count("COPY --from=package-probe-"), 3)
        self.assertNotIn("cargo build", rendered)
        self.assertNotIn("type=cache", rendered)
        self.assertIn('Acquire::https::CaInfo "/etc/ssl/certs/ca-certificates.crt";', rendered)
        self.assertIn('APT::Update::Error-Mode "any";', rendered)
        changed = PROBE.probe_dockerfile(source.replace("ARG FFMPEG_VERSION=", "ARG FFMPEG_VERSION=probe-")
                                        .replace("        build-essential \\\n", "        new-build-package \\\n"))
        self.assertEqual(changed.count("ARG FFMPEG_VERSION=probe-"), 2)
        self.assertIn("new-build-package", changed)
        self.assertNotIn("build-essential", changed)

    def test_unrecognized_graph_cannot_silently_report_partial_success(self):
        source = (REPO / "Dockerfile").read_text()
        with self.assertRaisesRegex(ValueError, "unrecognized package install"):
            PROBE.probe_dockerfile(source.replace("apt-get install -y", "apt-get install --yes", 1))
        with self.assertRaisesRegex(ValueError, "missing exact package argument"):
            PROBE.probe_dockerfile(source.replace("ARG FFMPEG_VERSION=", "ARG CHANGED_VERSION=", 1))


if __name__ == "__main__":
    unittest.main()
