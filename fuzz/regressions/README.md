# Promoted fuzz regressions

Each target gets a subdirectory containing minimized, content-addressed inputs.
The pull-request smoke workflow replays these inputs before starting mutation.

Promote a retained CI crash with:

```sh
scripts/promote-fuzz-regression.sh <target> <artifact> [sanitizer]
```

Review and commit the resulting `fuzz/regressions/<target>/<sha256>` file with
the parser fix. A regression input is successful only after it stops crashing.
